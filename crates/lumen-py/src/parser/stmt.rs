//! Statement parsing.

use std::rc::Rc;

use super::expr::{describe, set_ctx};
use super::{mk, PResult, Parser};
use crate::ast::*;
use crate::lexer::Tok;

fn stmt(pos: Pos, kind: StmtKind) -> Stmt {
    Stmt { pos, kind }
}

fn aug_op(op: &str) -> Option<BinOp> {
    Some(match op {
        "+=" => BinOp::Add,
        "-=" => BinOp::Sub,
        "*=" => BinOp::Mult,
        "@=" => BinOp::MatMult,
        "/=" => BinOp::Div,
        "%=" => BinOp::Mod,
        "&=" => BinOp::BitAnd,
        "|=" => BinOp::BitOr,
        "^=" => BinOp::BitXor,
        "<<=" => BinOp::LShift,
        ">>=" => BinOp::RShift,
        "**=" => BinOp::Pow,
        "//=" => BinOp::FloorDiv,
        _ => return None,
    })
}

impl Parser {
    pub(super) fn statement(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        let pos = self.pos();
        match self.tok() {
            Tok::Kw("if") => out.push(self.if_stmt()?),
            Tok::Kw("while") => out.push(self.while_stmt()?),
            Tok::Kw("for") => out.push(self.for_stmt(pos, false)?),
            Tok::Kw("try") => out.push(self.try_stmt()?),
            Tok::Kw("with") => out.push(self.with_stmt(pos, false)?),
            Tok::Kw("def") => out.push(self.funcdef(pos, Vec::new(), false)?),
            Tok::Kw("class") => out.push(self.classdef(pos, Vec::new())?),
            Tok::Kw("async") => out.push(self.async_stmt(Vec::new())?),
            Tok::Op("@") => out.push(self.decorated()?),
            Tok::Name(n) if &**n == "match" => {
                if !self.try_match(out)? {
                    self.simple_stmts(out)?;
                }
            }
            _ => self.simple_stmts(out)?,
        }
        Ok(())
    }

    fn block(&mut self, what: &str, line: u32) -> PResult<Vec<Stmt>> {
        self.expect_op(":")?;
        let mut body = Vec::new();
        if !matches!(self.tok(), Tok::Newline) {
            self.simple_stmts(&mut body)?;
            return Ok(body);
        }
        self.advance();
        if !matches!(self.tok(), Tok::Indent) {
            return self.error(format!(
                "expected an indented block after {what} on line {line}"
            ));
        }
        self.advance();
        while !matches!(self.tok(), Tok::Dedent | Tok::EndMarker) {
            if matches!(self.tok(), Tok::Newline) {
                self.advance();
                continue;
            }
            self.statement(&mut body)?;
        }
        self.advance();
        Ok(body)
    }

    fn simple_stmts(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        loop {
            self.simple_stmt(out)?;
            if self.eat_op(";") {
                if matches!(self.tok(), Tok::Newline) {
                    break;
                }
                continue;
            }
            break;
        }
        if matches!(self.tok(), Tok::Newline) {
            self.advance();
            Ok(())
        } else if matches!(self.tok(), Tok::EndMarker) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn at_stmt_end(&self) -> bool {
        matches!(self.tok(), Tok::Newline | Tok::EndMarker) || self.at_op(";")
    }

    fn simple_stmt(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        let pos = self.pos();
        let kind = match self.tok() {
            Tok::Kw("pass") => {
                self.advance();
                StmtKind::Pass
            }
            Tok::Kw("break") => {
                self.advance();
                StmtKind::Break
            }
            Tok::Kw("continue") => {
                self.advance();
                StmtKind::Continue
            }
            Tok::Kw("return") => {
                self.advance();
                let value = if self.at_stmt_end() {
                    None
                } else {
                    Some(self.star_expressions()?)
                };
                StmtKind::Return(value)
            }
            Tok::Kw("raise") => {
                self.advance();
                let mut exc = None;
                let mut cause = None;
                if !self.at_stmt_end() {
                    exc = Some(self.expression()?);
                    if self.eat_kw("from") {
                        cause = Some(self.expression()?);
                    }
                }
                StmtKind::Raise { exc, cause }
            }
            Tok::Kw("global") => {
                self.advance();
                StmtKind::Global(self.name_list()?)
            }
            Tok::Kw("nonlocal") => {
                self.advance();
                StmtKind::Nonlocal(self.name_list()?)
            }
            Tok::Kw("del") => {
                self.advance();
                self.del_stmt()?
            }
            Tok::Kw("assert") => {
                self.advance();
                let test = self.expression()?;
                let msg = if self.eat_op(",") {
                    Some(self.expression()?)
                } else {
                    None
                };
                StmtKind::Assert { test, msg }
            }
            Tok::Kw("import") => {
                self.advance();
                self.import_stmt()?
            }
            Tok::Kw("from") => {
                self.advance();
                self.import_from()?
            }
            Tok::Name(n)
                if &**n == "type"
                    && matches!(self.peek(1), Tok::Name(_))
                    && matches!(self.peek(2), Tok::Op("=") | Tok::Op("[")) =>
            {
                self.advance();
                let npos = self.pos();
                let id = self.ident()?;
                let name = mk(npos, ExprKind::Name { id, ctx: Ctx::Store });
                let type_params = self.type_params()?;
                self.expect_op("=")?;
                let value = self.expression()?;
                StmtKind::TypeAlias { name, type_params, value }
            }
            _ => return self.expr_stmt(out),
        };
        out.push(stmt(pos, kind));
        Ok(())
    }

    fn name_list(&mut self) -> PResult<Vec<Ident>> {
        let mut names = vec![self.ident()?];
        while self.eat_op(",") {
            names.push(self.ident()?);
        }
        Ok(names)
    }

    fn del_stmt(&mut self) -> PResult<StmtKind> {
        let mut targets = Vec::new();
        loop {
            let mut t = self.binary(1)?;
            set_ctx(&mut t, Ctx::Del)?;
            targets.push(t);
            if !self.eat_op(",") || self.at_stmt_end() {
                break;
            }
        }
        Ok(StmtKind::Delete(targets))
    }

    fn dotted_name(&mut self) -> PResult<Ident> {
        let mut name = self.ident()?.to_string();
        while self.at_op(".") {
            self.advance();
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(Rc::from(name.as_str()))
    }

    fn import_stmt(&mut self) -> PResult<StmtKind> {
        let mut names = Vec::new();
        loop {
            let name = self.dotted_name()?;
            let asname = if self.eat_kw("as") {
                Some(self.ident()?)
            } else {
                None
            };
            names.push(Alias { name, asname });
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(StmtKind::Import(names))
    }

    fn import_from(&mut self) -> PResult<StmtKind> {
        let mut level = 0;
        loop {
            if self.eat_op(".") {
                level += 1;
            } else if self.eat_op("...") {
                level += 3;
            } else {
                break;
            }
        }
        let module = if self.at_kw("import") && level > 0 {
            None
        } else {
            Some(self.dotted_name()?)
        };
        self.expect_kw("import")?;
        let mut names = Vec::new();
        if self.at_op("*") {
            self.advance();
            names.push(Alias {
                name: Rc::from("*"),
                asname: None,
            });
        } else {
            let paren = self.eat_op("(");
            loop {
                let name = self.ident()?;
                let asname = if self.eat_kw("as") {
                    Some(self.ident()?)
                } else {
                    None
                };
                names.push(Alias { name, asname });
                if !self.eat_op(",") {
                    break;
                }
                if paren && self.at_op(")") {
                    break;
                }
                if !paren && self.at_stmt_end() {
                    return self
                        .error("trailing comma not allowed without surrounding parentheses");
                }
            }
            if paren {
                self.expect_op(")")?;
            }
        }
        Ok(StmtKind::ImportFrom {
            module,
            names,
            level,
        })
    }

    fn rhs(&mut self) -> PResult<Expr> {
        if self.at_kw("yield") {
            self.yield_expr()
        } else {
            self.star_expressions()
        }
    }

    fn expr_stmt(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        let pos = self.pos();
        let starts_with_name = matches!(self.tok(), Tok::Name(_));
        let first = self.rhs()?;
        let kind = match self.tok() {
            Tok::Op(":") => {
                if !matches!(
                    first.kind,
                    ExprKind::Name { .. } | ExprKind::Attribute { .. } | ExprKind::Subscript { .. }
                ) {
                    let what = match first.kind {
                        ExprKind::Tuple { .. } => "only single target (not tuple) can be annotated",
                        ExprKind::List { .. } => "only single target (not list) can be annotated",
                        _ => "illegal target for annotation",
                    };
                    return self.error_at(first.pos, what);
                }
                self.advance();
                let annotation = self.expression()?;
                let value = if self.eat_op("=") {
                    Some(self.rhs()?)
                } else {
                    None
                };
                let simple = starts_with_name && matches!(first.kind, ExprKind::Name { .. });
                let mut target = first;
                set_ctx(&mut target, Ctx::Store)?;
                StmtKind::AnnAssign {
                    target,
                    annotation,
                    value,
                    simple,
                }
            }
            Tok::Op(op) if aug_op(op).is_some() => {
                let op = aug_op(op).unwrap_or(BinOp::Add);
                if !matches!(
                    first.kind,
                    ExprKind::Name { .. } | ExprKind::Attribute { .. } | ExprKind::Subscript { .. }
                ) {
                    return self.error_at(
                        first.pos,
                        format!(
                            "'{}' is an illegal expression for augmented assignment",
                            describe(&first.kind)
                        ),
                    );
                }
                self.advance();
                let value = self.rhs()?;
                let mut target = first;
                set_ctx(&mut target, Ctx::Store)?;
                StmtKind::AugAssign { target, op, value }
            }
            Tok::Op("=") => {
                let mut targets = vec![first];
                let value;
                loop {
                    self.advance();
                    let v = self.rhs()?;
                    if self.at_op("=") {
                        targets.push(v);
                    } else {
                        value = v;
                        break;
                    }
                }
                for t in &mut targets {
                    set_ctx(t, Ctx::Store)?;
                }
                StmtKind::Assign { targets, value }
            }
            _ => StmtKind::Expr(first),
        };
        out.push(stmt(pos, kind));
        Ok(())
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        let mut clauses = Vec::new();
        let mut orelse = Vec::new();
        loop {
            if clauses.len() as u32 > super::MAX_CHAIN {
                return self.error("too many nested expressions");
            }
            let cpos = self.pos();
            let is_if = self.at_kw("if");
            self.advance();
            let test = self.named_expression()?;
            let what = if is_if {
                "'if' statement"
            } else {
                "'elif' statement"
            };
            let body = self.block(what, cpos.line)?;
            clauses.push((cpos, test, body));
            if self.at_kw("elif") {
                continue;
            }
            if self.at_kw("else") {
                let line = self.pos().line;
                self.advance();
                orelse = self.block("'else' statement", line)?;
            }
            break;
        }
        while let Some((cpos, test, body)) = clauses.pop() {
            let s = stmt(cpos, StmtKind::If { test, body, orelse });
            orelse = vec![s];
        }
        match orelse.pop() {
            Some(s) => Ok(s),
            None => Err(self.unexpected()),
        }
    }

    fn while_stmt(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        self.advance();
        let test = self.named_expression()?;
        let body = self.block("'while' statement", pos.line)?;
        let orelse = self.else_block()?;
        Ok(stmt(pos, StmtKind::While { test, body, orelse }))
    }

    fn else_block(&mut self) -> PResult<Vec<Stmt>> {
        if self.at_kw("else") {
            let line = self.pos().line;
            self.advance();
            self.block("'else' statement", line)
        } else {
            Ok(Vec::new())
        }
    }

    fn for_stmt(&mut self, pos: Pos, is_async: bool) -> PResult<Stmt> {
        self.advance();
        let target = self.star_targets()?;
        self.expect_kw("in")?;
        let iter = self.star_expressions()?;
        let body = self.block("'for' statement", pos.line)?;
        let orelse = self.else_block()?;
        Ok(stmt(
            pos,
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                is_async,
            },
        ))
    }

    fn try_stmt(&mut self) -> PResult<Stmt> {
        let pos = self.pos();
        self.advance();
        let body = self.block("'try' statement", pos.line)?;
        let mut handlers = Vec::new();
        let mut is_star = None;
        while self.at_kw("except") {
            let hpos = self.pos();
            self.advance();
            let star = self.eat_op("*");
            match is_star {
                None => is_star = Some(star),
                Some(s) if s != star => {
                    return self.error_at(
                        hpos,
                        "cannot have both 'except' and 'except*' on the same 'try'",
                    );
                }
                _ => {}
            }
            let mut typ = None;
            let mut name = None;
            if !self.at_op(":") {
                let tpos = self.pos();
                let first = self.expression()?;
                if self.at_op(",") {
                    let mut elts = vec![first];
                    while self.eat_op(",") {
                        if self.at_op(":") {
                            break;
                        }
                        elts.push(self.expression()?);
                    }
                    typ = Some(mk(
                        tpos,
                        ExprKind::Tuple {
                            elts,
                            ctx: Ctx::Load,
                        },
                    ));
                } else {
                    typ = Some(first);
                    if self.eat_kw("as") {
                        name = Some(self.ident()?);
                    }
                }
            } else if star {
                return self.error("expected one or more exception types");
            }
            let what = if star {
                "'except*' statement"
            } else {
                "'except' statement"
            };
            let hbody = self.block(what, hpos.line)?;
            handlers.push(ExceptHandler {
                pos: hpos,
                typ,
                name,
                body: hbody,
            });
        }
        let orelse = if handlers.is_empty() {
            Vec::new()
        } else {
            self.else_block()?
        };
        let mut finalbody = Vec::new();
        if self.at_kw("finally") {
            let line = self.pos().line;
            self.advance();
            finalbody = self.block("'finally' statement", line)?;
        }
        if handlers.is_empty() && finalbody.is_empty() {
            return self.error("expected 'except' or 'finally' block");
        }
        Ok(stmt(
            pos,
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
                is_star: is_star.unwrap_or(false),
            },
        ))
    }

    fn with_item(&mut self) -> PResult<WithItem> {
        let context_expr = self.expression()?;
        let optional_vars = if self.eat_kw("as") {
            let mut t = self.star_target()?;
            set_ctx(&mut t, Ctx::Store)?;
            Some(t)
        } else {
            None
        };
        Ok(WithItem {
            context_expr,
            optional_vars,
        })
    }

    fn paren_with_items(&mut self) -> PResult<Vec<WithItem>> {
        self.advance();
        let mut items = Vec::new();
        loop {
            items.push(self.with_item()?);
            if !self.eat_op(",") || self.at_op(")") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(items)
    }

    fn with_stmt(&mut self, pos: Pos, is_async: bool) -> PResult<Stmt> {
        self.advance();
        let mut items = None;
        if self.at_op("(") {
            let save = self.p;
            let saved_depth = self.depth;
            match self.paren_with_items() {
                Ok(i) if self.at_op(":") => items = Some(i),
                _ => {
                    self.p = save;
                    self.depth = saved_depth;
                }
            }
        }
        let items = match items {
            Some(i) => i,
            None => {
                let mut items = vec![self.with_item()?];
                while self.eat_op(",") {
                    items.push(self.with_item()?);
                }
                items
            }
        };
        let body = self.block("'with' statement", pos.line)?;
        Ok(stmt(
            pos,
            StmtKind::With {
                items,
                body,
                is_async,
            },
        ))
    }

    fn async_stmt(&mut self, decorators: Vec<Expr>) -> PResult<Stmt> {
        let pos = self.pos();
        self.advance();
        match self.tok() {
            Tok::Kw("def") => self.funcdef(pos, decorators, true),
            Tok::Kw("for") if decorators.is_empty() => self.for_stmt(pos, true),
            Tok::Kw("with") if decorators.is_empty() => self.with_stmt(pos, true),
            _ => Err(self.unexpected()),
        }
    }

    fn decorated(&mut self) -> PResult<Stmt> {
        let mut decorators = Vec::new();
        while self.at_op("@") {
            self.advance();
            decorators.push(self.named_expression()?);
            if !matches!(self.tok(), Tok::Newline) {
                return Err(self.unexpected());
            }
            self.advance();
        }
        let pos = self.pos();
        match self.tok() {
            Tok::Kw("def") => self.funcdef(pos, decorators, false),
            Tok::Kw("class") => self.classdef(pos, decorators),
            Tok::Kw("async") => self.async_stmt(decorators),
            _ => Err(self.unexpected()),
        }
    }

    fn type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut params = Vec::new();
        if !self.eat_op("[") {
            return Ok(params);
        }
        loop {
            let pos = self.pos();
            let kind = if self.eat_op("*") {
                let name = self.ident()?;
                if self.at_op(":") {
                    return self.error("cannot use bound with TypeVarTuple");
                }
                TypeParamKind::TypeVarTuple { name }
            } else if self.eat_op("**") {
                let name = self.ident()?;
                if self.at_op(":") {
                    return self.error("cannot use bound with ParamSpec");
                }
                TypeParamKind::ParamSpec { name }
            } else {
                let name = self.ident()?;
                let bound = if self.eat_op(":") {
                    Some(self.expression()?)
                } else {
                    None
                };
                TypeParamKind::TypeVar { name, bound }
            };
            params.push(TypeParam { pos, kind });
            if !self.eat_op(",") || self.at_op("]") {
                break;
            }
        }
        self.expect_op("]")?;
        Ok(params)
    }

    fn funcdef(&mut self, pos: Pos, decorators: Vec<Expr>, is_async: bool) -> PResult<Stmt> {
        self.advance();
        let name = self.ident()?;
        let type_params = self.type_params()?;
        self.expect_op("(")?;
        let args = self.parameters(false)?;
        self.expect_op(")")?;
        let returns = if self.eat_op("->") {
            Some(self.expression()?)
        } else {
            None
        };
        let body = self.block("function definition", pos.line)?;
        Ok(stmt(
            pos,
            StmtKind::FunctionDef(Box::new(FunctionDef {
                name,
                args,
                body,
                decorators,
                returns,
                is_async,
                type_params,
            })),
        ))
    }

    fn classdef(&mut self, pos: Pos, decorators: Vec<Expr>) -> PResult<Stmt> {
        self.advance();
        let name = self.ident()?;
        let type_params = self.type_params()?;
        let (mut bases, mut keywords) = (Vec::new(), Vec::new());
        let lparen = self.pos();
        if self.eat_op("(") {
            (bases, keywords) = self.call_args(lparen)?;
            self.expect_op(")")?;
        }
        let body = self.block("class definition", pos.line)?;
        Ok(stmt(
            pos,
            StmtKind::ClassDef(Box::new(ClassDef {
                name,
                bases,
                keywords,
                body,
                decorators,
                type_params,
            })),
        ))
    }

    /// Tries to parse a `match` statement; returns false (with the position restored) when the
    /// `match` token turns out to be an ordinary identifier.
    fn try_match(&mut self, out: &mut Vec<Stmt>) -> PResult<bool> {
        let pos = self.pos();
        let (save, saved_depth) = (self.p, self.depth);
        self.advance();
        let subject = match self.match_subject() {
            Ok(s) if self.at_op(":") && matches!(self.peek(1), Tok::Newline) => s,
            _ => {
                self.p = save;
                self.depth = saved_depth;
                return Ok(false);
            }
        };
        self.advance();
        self.advance();
        if !matches!(self.tok(), Tok::Indent) {
            return self.error(format!(
                "expected an indented block after 'match' statement on line {}",
                pos.line
            ));
        }
        self.advance();
        let mut cases = Vec::new();
        while self.at_name("case") {
            let cpos = self.pos();
            self.advance();
            let pattern = self.patterns()?;
            let guard = if self.eat_kw("if") {
                Some(self.named_expression()?)
            } else {
                None
            };
            let body = self.block("'case' statement", cpos.line)?;
            cases.push(MatchCase {
                pattern,
                guard,
                body,
            });
        }
        if cases.is_empty() {
            return self.error("expected 'case' block");
        }
        if !matches!(self.tok(), Tok::Dedent) {
            return Err(self.unexpected());
        }
        self.advance();
        out.push(stmt(pos, StmtKind::Match { subject, cases }));
        Ok(true)
    }

    fn match_subject(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.star_named_expression()?;
        if !self.at_op(",") {
            if matches!(first.kind, ExprKind::Starred { .. }) {
                return Err(self.unexpected());
            }
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.at_op(":") {
                break;
            }
            elts.push(self.star_named_expression()?);
        }
        Ok(mk(
            pos,
            ExprKind::Tuple {
                elts,
                ctx: Ctx::Load,
            },
        ))
    }
}
