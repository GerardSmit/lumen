//! Syntax-tree traversal helpers shared by the checker's pre-passes.

use super::ast::*;

/// Calls `f` on each direct child expression of `e`. Function and class bodies are not
/// children (they are separate `Func`s / class members); a class expression's `extends`
/// clause and computed keys are.
pub fn for_each_child<'a>(m: &'a Module, e: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
    match &e.kind {
        ExprKind::Template(parts) => parts.iter().for_each(f),
        ExprKind::TaggedTemplate(tag, parts) => {
            f(tag);
            parts.iter().for_each(f);
        }
        ExprKind::Array(items) => items.iter().flatten().for_each(f),
        ExprKind::Object(props) => {
            for p in props {
                match p {
                    Prop::KeyValue { key, value } => {
                        if let PropKey::Computed(k) = key {
                            f(k);
                        }
                        f(value);
                    }
                    Prop::Method { key, .. } => {
                        if let PropKey::Computed(k) = key {
                            f(k);
                        }
                    }
                    Prop::Spread(x) => f(x),
                    Prop::Shorthand { .. } => {}
                }
            }
        }
        ExprKind::Class(id) => {
            if let Some(ext) = &m.classes[*id].extends {
                f(ext);
            }
        }
        ExprKind::Member { obj, .. } => f(obj),
        ExprKind::Index { obj, index, .. } => {
            f(obj);
            f(index);
        }
        ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args, .. } => {
            f(callee);
            args.iter().for_each(f);
        }
        ExprKind::Unary { arg, .. } | ExprKind::Update { arg, .. } => f(arg),
        ExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        ExprKind::Assign { target, value, .. } => {
            f(target);
            f(value);
        }
        ExprKind::Cond { test, cons, alt } => {
            f(test);
            f(cons);
            f(alt);
        }
        ExprKind::Seq(items) => items.iter().for_each(f),
        ExprKind::Spread(x)
        | ExprKind::As { expr: x, .. }
        | ExprKind::Satisfies { expr: x, .. }
        | ExprKind::NonNull(x)
        | ExprKind::TypeAssert { expr: x, .. }
        | ExprKind::JsDocCast { expr: x, .. }
        | ExprKind::Paren(x)
        | ExprKind::Await(x)
        | ExprKind::ImportCall(x) => f(x),
        ExprKind::Yield(x) => {
            if let Some(x) = x {
                f(x);
            }
        }
        ExprKind::Num(_)
        | ExprKind::Str(_)
        | ExprKind::BigInt(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Regex
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::Func(_)
        | ExprKind::MetaProp
        | ExprKind::Unsupported(_) => {}
    }
}

/// Pre-order traversal over expressions reachable from statements, optionally descending into
/// nested arrow functions (which share `this`) or into all nested functions and classes.
pub struct Walk<'m> {
    pub m: &'m Module,
    pub into_arrows: bool,
    pub into_fns: bool,
}

impl<'m> Walk<'m> {
    pub fn func(&self, id: FnId, f: &mut dyn FnMut(&'m Expr)) {
        let func = &self.m.funcs[id];
        for p in &func.params {
            if let Some(d) = &p.default {
                self.expr(d, f);
            }
            if let Pattern::Destructure { defaults, .. } = &p.pat {
                defaults.iter().for_each(|d| self.expr(d, f));
            }
        }
        match &func.body {
            Some(Body::Block(ss)) => self.stmts(ss, f),
            Some(Body::Expr(e)) => self.expr(e, f),
            None => {}
        }
    }

    pub fn class(&self, id: ClassId, f: &mut dyn FnMut(&'m Expr)) {
        let c = &self.m.classes[id];
        if let Some(e) = &c.extends {
            self.expr(e, f);
        }
        if !self.into_fns {
            return;
        }
        for mem in &c.members {
            match mem {
                Member::Field { init: Some(e), .. } => self.expr(e, f),
                Member::Method { func, .. } => self.func(*func, f),
                Member::StaticBlock(ss, _) => self.stmts(ss, f),
                _ => {}
            }
        }
    }

    pub fn expr(&self, e: &'m Expr, f: &mut dyn FnMut(&'m Expr)) {
        f(e);
        match &e.kind {
            ExprKind::Func(id) => {
                if self.into_fns || (self.into_arrows && self.m.funcs[*id].kind == FnKind::Arrow) {
                    self.func(*id, f);
                }
            }
            ExprKind::Class(id) => self.class(*id, f),
            ExprKind::Object(props) if self.into_fns => {
                for p in props {
                    if let Prop::Method { func, .. } = p {
                        self.func(*func, f);
                    }
                }
                for_each_child(self.m, e, &mut |c| self.expr(c, f));
            }
            _ => for_each_child(self.m, e, &mut |c| self.expr(c, f)),
        }
    }

    pub fn stmts(&self, ss: &'m [Stmt], f: &mut dyn FnMut(&'m Expr)) {
        for s in ss {
            self.stmt(s, f);
        }
    }

    pub fn stmt(&self, s: &'m Stmt, f: &mut dyn FnMut(&'m Expr)) {
        match s {
            Stmt::Var { decls, .. } => {
                for d in decls {
                    if let Pattern::Destructure { defaults, .. } = &d.pat {
                        defaults.iter().for_each(|x| self.expr(x, f));
                    }
                    if let Some(i) = &d.init {
                        self.expr(i, f);
                    }
                }
            }
            Stmt::Func(id) => {
                if self.into_fns {
                    self.func(*id, f);
                }
            }
            Stmt::Class(id) => self.class(*id, f),
            Stmt::Enum { members, .. } => {
                for (_, init) in members {
                    if let Some(i) = init {
                        self.expr(i, f);
                    }
                }
            }
            Stmt::Namespace { body, .. } => self.stmts(body, f),
            Stmt::Export { stmt, .. } => self.stmt(stmt, f),
            Stmt::ExportDefaultExpr { expr, .. } | Stmt::ExportAssign { expr, .. } => {
                self.expr(expr, f)
            }
            Stmt::Expr(e) => self.expr(e, f),
            Stmt::Block(ss, _) => self.stmts(ss, f),
            Stmt::If {
                test, cons, alt, ..
            } => {
                self.expr(test, f);
                self.stmt(cons, f);
                if let Some(a) = alt {
                    self.stmt(a, f);
                }
            }
            Stmt::For {
                init,
                test,
                update,
                body,
                ..
            } => {
                if let Some(i) = init {
                    self.stmt(i, f);
                }
                if let Some(t) = test {
                    self.expr(t, f);
                }
                if let Some(u) = update {
                    self.expr(u, f);
                }
                self.stmt(body, f);
            }
            Stmt::ForIn {
                head, right, body, ..
            } => {
                match head {
                    ForHead::Expr(e) => self.expr(e, f),
                    ForHead::Var(_, Pattern::Destructure { defaults, .. }) => {
                        defaults.iter().for_each(|x| self.expr(x, f))
                    }
                    ForHead::Var(..) => {}
                }
                self.expr(right, f);
                self.stmt(body, f);
            }
            Stmt::While { test, body, .. } | Stmt::DoWhile { body, test, .. } => {
                self.expr(test, f);
                self.stmt(body, f);
            }
            Stmt::Return(e, _) => {
                if let Some(e) = e {
                    self.expr(e, f);
                }
            }
            Stmt::Throw(e, _) => self.expr(e, f),
            Stmt::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                self.stmts(block, f);
                if let Some(h) = handler {
                    self.stmts(h, f);
                }
                if let Some(fin) = finalizer {
                    self.stmts(fin, f);
                }
            }
            Stmt::Switch { disc, cases, .. } => {
                self.expr(disc, f);
                for (t, body) in cases {
                    if let Some(t) = t {
                        self.expr(t, f);
                    }
                    self.stmts(body, f);
                }
            }
            Stmt::Labeled { body, .. } => self.stmt(body, f),
            Stmt::With { obj, body, .. } => {
                self.expr(obj, f);
                self.stmt(body, f);
            }
            Stmt::TypeAlias { .. }
            | Stmt::Interface { .. }
            | Stmt::Import { .. }
            | Stmt::ImportEquals { .. }
            | Stmt::ExportNamed { .. }
            | Stmt::Break(..)
            | Stmt::Continue(..)
            | Stmt::Empty(_)
            | Stmt::Debugger(_) => {}
        }
    }
}

/// Names a function body assigns (not descending into nested functions), plus whether it has
/// writes the parser could not attribute to names (destructuring assignment).
pub fn own_assigned(m: &Module, id: FnId) -> (Vec<String>, bool) {
    let mut names = Vec::new();
    let mut unknown = false;
    let w = Walk {
        m,
        into_arrows: false,
        into_fns: false,
    };
    w.func(id, &mut |e| collect_assigned(e, &mut names, &mut unknown));
    (names, unknown)
}

pub fn collect_assigned(e: &Expr, names: &mut Vec<String>, unknown: &mut bool) {
    let target = match &e.kind {
        ExprKind::Assign { target, .. } => Some(target),
        ExprKind::Update { arg, .. } => Some(arg),
        _ => None,
    };
    if let Some(t) = target {
        let mut t: &Expr = t;
        while let ExprKind::Paren(inner) | ExprKind::NonNull(inner) = &t.kind {
            t = inner;
        }
        match &t.kind {
            ExprKind::Ident(n) => names.push(n.clone()),
            ExprKind::Unsupported(_) => *unknown = true,
            _ => {}
        }
    }
}

/// Whether statements (or a statement tree) contain a `break` that could leave the enclosing
/// loop/switch (unlabeled breaks outside nested loops/switches, or any labeled break).
pub fn has_break(s: &Stmt, nested: bool) -> bool {
    match s {
        Stmt::Break(label, _) => label.is_some() || !nested,
        Stmt::Block(ss, _) => ss.iter().any(|s| has_break(s, nested)),
        Stmt::If { cons, alt, .. } => {
            has_break(cons, nested) || alt.as_ref().is_some_and(|a| has_break(a, nested))
        }
        Stmt::Labeled { body, .. } => has_break(body, nested),
        Stmt::Try {
            block,
            handler,
            finalizer,
            ..
        } => {
            block.iter().any(|s| has_break(s, nested))
                || handler
                    .as_ref()
                    .is_some_and(|h| h.iter().any(|s| has_break(s, nested)))
                || finalizer
                    .as_ref()
                    .is_some_and(|h| h.iter().any(|s| has_break(s, nested)))
        }
        Stmt::For { body, .. }
        | Stmt::ForIn { body, .. }
        | Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. } => has_break(body, true),
        Stmt::Switch { cases, .. } => cases
            .iter()
            .any(|(_, b)| b.iter().any(|s| has_break(s, true))),
        _ => false,
    }
}
