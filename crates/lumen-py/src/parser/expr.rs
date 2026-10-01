//! Expression parsing.

use super::{mk, PResult, Parser, MAX_CHAIN};
use crate::ast::*;
use crate::lexer::Tok;

pub(super) fn describe(kind: &ExprKind) -> &'static str {
    match kind {
        ExprKind::Call { .. } => "function call",
        ExprKind::Constant(Constant::None) => "None",
        ExprKind::Constant(Constant::True) => "True",
        ExprKind::Constant(Constant::False) => "False",
        ExprKind::Constant(Constant::Ellipsis) => "ellipsis",
        ExprKind::Constant(_) => "literal",
        ExprKind::BinOp { .. } | ExprKind::UnaryOp { .. } | ExprKind::BoolOp { .. } => "expression",
        ExprKind::Compare { .. } => "comparison",
        ExprKind::Lambda { .. } => "lambda",
        ExprKind::IfExp { .. } => "conditional expression",
        ExprKind::Dict { .. } => "dict literal",
        ExprKind::Set(_) => "set display",
        ExprKind::ListComp { .. } => "list comprehension",
        ExprKind::SetComp { .. } => "set comprehension",
        ExprKind::DictComp { .. } => "dict comprehension",
        ExprKind::GeneratorExp { .. } => "generator expression",
        ExprKind::JoinedStr(_) | ExprKind::FormattedValue { .. } => "f-string expression",
        ExprKind::Await(_) => "await expression",
        ExprKind::Yield(_) | ExprKind::YieldFrom(_) => "yield expression",
        ExprKind::NamedExpr { .. } => "named expression",
        ExprKind::Attribute { .. } => "attribute",
        ExprKind::Subscript { .. } => "subscript",
        ExprKind::Starred { .. } => "starred",
        ExprKind::Name { .. } => "name",
        ExprKind::List { .. } => "list",
        ExprKind::Tuple { .. } => "tuple",
        ExprKind::Slice { .. } => "slice",
    }
}

pub(super) fn set_ctx(e: &mut Expr, ctx: Ctx) -> PResult<()> {
    match &mut e.kind {
        ExprKind::Name { ctx: c, .. }
        | ExprKind::Attribute { ctx: c, .. }
        | ExprKind::Subscript { ctx: c, .. } => {
            *c = ctx;
            Ok(())
        }
        ExprKind::Starred { value, ctx: c } => {
            if ctx == Ctx::Del {
                return Err(err_at(e.pos, "cannot delete starred"));
            }
            *c = ctx;
            set_ctx(value, ctx)
        }
        ExprKind::Tuple { elts, ctx: c } | ExprKind::List { elts, ctx: c } => {
            *c = ctx;
            for elt in elts {
                set_ctx(elt, ctx)?;
            }
            Ok(())
        }
        other => {
            let verb = if ctx == Ctx::Del {
                "delete"
            } else {
                "assign to"
            };
            Err(err_at(e.pos, format!("cannot {verb} {}", describe(other))))
        }
    }
}

fn err_at(pos: Pos, msg: impl Into<String>) -> super::SyntaxError {
    super::SyntaxError {
        msg: msg.into(),
        line: pos.line,
        col: pos.col,
    }
}

fn can_start_expr(t: &Tok) -> bool {
    match t {
        Tok::Name(_)
        | Tok::Int(_)
        | Tok::Float(_)
        | Tok::Imag(_)
        | Tok::Str(_)
        | Tok::Bytes(_)
        | Tok::FStr { .. } => true,
        Tok::Kw(k) => matches!(*k, "None" | "True" | "False" | "not" | "lambda" | "await"),
        Tok::Op(o) => matches!(*o, "(" | "[" | "{" | "-" | "+" | "~" | "..." | "*"),
        _ => false,
    }
}

impl Parser {
    pub(super) fn can_start_expr(&self) -> bool {
        can_start_expr(self.tok())
    }

    pub(super) fn star_expressions(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.star_expression()?;
        if !self.at_op(",") {
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if !self.can_start_expr() {
                break;
            }
            elts.push(self.star_expression()?);
        }
        Ok(mk(
            pos,
            ExprKind::Tuple {
                elts,
                ctx: Ctx::Load,
            },
        ))
    }

    pub(super) fn star_expression(&mut self) -> PResult<Expr> {
        if self.at_op("*") {
            return self.starred(false);
        }
        self.expression()
    }

    pub(super) fn star_named_expression(&mut self) -> PResult<Expr> {
        if self.at_op("*") {
            return self.starred(false);
        }
        self.named_expression()
    }

    fn starred(&mut self, full: bool) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        let value = if full {
            self.expression()?
        } else {
            self.binary(1)?
        };
        Ok(mk(
            pos,
            ExprKind::Starred {
                value: Box::new(value),
                ctx: Ctx::Load,
            },
        ))
    }

    pub(super) fn named_expression(&mut self) -> PResult<Expr> {
        if let Tok::Name(n) = self.tok() {
            if matches!(self.peek(1), Tok::Op(":=")) {
                let (n, pos) = (n.clone(), self.pos());
                self.advance();
                self.advance();
                let value = self.expression()?;
                let target = mk(
                    pos,
                    ExprKind::Name {
                        id: n,
                        ctx: Ctx::Store,
                    },
                );
                return Ok(mk(
                    pos,
                    ExprKind::NamedExpr {
                        target: Box::new(target),
                        value: Box::new(value),
                    },
                ));
            }
        }
        self.expression()
    }

    pub(super) fn expression(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.expression_inner();
        self.leave();
        r
    }

    fn expression_inner(&mut self) -> PResult<Expr> {
        if self.at_kw("lambda") {
            return self.lambda();
        }
        let pos = self.pos();
        let body = self.disjunction()?;
        if !self.at_kw("if") {
            return Ok(body);
        }
        self.advance();
        let test = self.disjunction()?;
        self.expect_kw("else")?;
        let orelse = self.expression()?;
        Ok(mk(
            pos,
            ExprKind::IfExp {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            },
        ))
    }

    fn lambda(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        let args = self.parameters(true)?;
        self.expect_op(":")?;
        let body = self.expression()?;
        Ok(mk(
            pos,
            ExprKind::Lambda {
                args: Box::new(args),
                body: Box::new(body),
            },
        ))
    }

    fn disjunction(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.conjunction()?;
        if !self.at_kw("or") {
            return Ok(first);
        }
        let mut values = vec![first];
        while self.eat_kw("or") {
            values.push(self.conjunction()?);
        }
        Ok(mk(
            pos,
            ExprKind::BoolOp {
                op: BoolOp::Or,
                values,
            },
        ))
    }

    fn conjunction(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.inversion()?;
        if !self.at_kw("and") {
            return Ok(first);
        }
        let mut values = vec![first];
        while self.eat_kw("and") {
            values.push(self.inversion()?);
        }
        Ok(mk(
            pos,
            ExprKind::BoolOp {
                op: BoolOp::And,
                values,
            },
        ))
    }

    fn inversion(&mut self) -> PResult<Expr> {
        if !self.at_kw("not") {
            return self.comparison();
        }
        self.enter()?;
        let pos = self.pos();
        self.advance();
        let r = self.inversion();
        self.leave();
        Ok(mk(
            pos,
            ExprKind::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(r?),
            },
        ))
    }

    fn comparison(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let left = self.binary(1)?;
        let mut ops = Vec::new();
        let mut comparators = Vec::new();
        loop {
            let op = match self.tok() {
                Tok::Op("==") => CmpOp::Eq,
                Tok::Op("!=") => CmpOp::NotEq,
                Tok::Op("<") => CmpOp::Lt,
                Tok::Op("<=") => CmpOp::LtE,
                Tok::Op(">") => CmpOp::Gt,
                Tok::Op(">=") => CmpOp::GtE,
                Tok::Kw("in") => CmpOp::In,
                Tok::Kw("not") if matches!(self.peek(1), Tok::Kw("in")) => {
                    self.advance();
                    CmpOp::NotIn
                }
                Tok::Kw("is") => {
                    if matches!(self.peek(1), Tok::Kw("not")) {
                        self.advance();
                        CmpOp::IsNot
                    } else {
                        CmpOp::Is
                    }
                }
                _ => break,
            };
            self.advance();
            ops.push(op);
            comparators.push(self.binary(1)?);
        }
        if ops.is_empty() {
            return Ok(left);
        }
        Ok(mk(
            pos,
            ExprKind::Compare {
                left: Box::new(left),
                ops,
                comparators,
            },
        ))
    }

    /// Precedence climbing over `|`, `^`, `&`, shifts, additive and multiplicative operators.
    pub(super) fn binary(&mut self, min: u8) -> PResult<Expr> {
        let pos = self.pos();
        let mut left = self.unary()?;
        let mut chain = 0;
        loop {
            let (op, prec) = match self.tok() {
                Tok::Op("|") => (BinOp::BitOr, 1),
                Tok::Op("^") => (BinOp::BitXor, 2),
                Tok::Op("&") => (BinOp::BitAnd, 3),
                Tok::Op("<<") => (BinOp::LShift, 4),
                Tok::Op(">>") => (BinOp::RShift, 4),
                Tok::Op("+") => (BinOp::Add, 5),
                Tok::Op("-") => (BinOp::Sub, 5),
                Tok::Op("*") => (BinOp::Mult, 6),
                Tok::Op("/") => (BinOp::Div, 6),
                Tok::Op("//") => (BinOp::FloorDiv, 6),
                Tok::Op("%") => (BinOp::Mod, 6),
                Tok::Op("@") => (BinOp::MatMult, 6),
                _ => break,
            };
            if prec < min {
                break;
            }
            chain += 1;
            if chain > MAX_CHAIN {
                return self.error("too many nested expressions");
            }
            self.advance();
            let right = self.binary(prec + 1)?;
            left = mk(
                pos,
                ExprKind::BinOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
            );
        }
        Ok(left)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let op = match self.tok() {
            Tok::Op("-") => UnaryOp::USub,
            Tok::Op("+") => UnaryOp::UAdd,
            Tok::Op("~") => UnaryOp::Invert,
            _ => return self.power(),
        };
        self.enter()?;
        let pos = self.pos();
        self.advance();
        let r = self.unary();
        self.leave();
        Ok(mk(
            pos,
            ExprKind::UnaryOp {
                op,
                operand: Box::new(r?),
            },
        ))
    }

    fn power(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let base = if self.at_kw("await") {
            let pos = self.pos();
            self.advance();
            let value = self.primary()?;
            mk(pos, ExprKind::Await(Box::new(value)))
        } else {
            self.primary()?
        };
        if !self.at_op("**") {
            return Ok(base);
        }
        self.advance();
        self.enter()?;
        let exp = self.unary();
        self.leave();
        let exp = exp?;
        Ok(mk(
            pos,
            ExprKind::BinOp {
                left: Box::new(base),
                op: BinOp::Pow,
                right: Box::new(exp),
            },
        ))
    }

    fn primary(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let mut e = self.atom()?;
        let mut chain = 0;
        loop {
            chain += 1;
            if chain > MAX_CHAIN {
                return self.error("too many nested expressions");
            }
            match self.tok() {
                Tok::Op(".") => {
                    self.advance();
                    let attr = self.ident()?;
                    e = mk(
                        pos,
                        ExprKind::Attribute {
                            value: Box::new(e),
                            attr,
                            ctx: Ctx::Load,
                        },
                    );
                }
                Tok::Op("(") => {
                    let lparen = self.pos();
                    self.advance();
                    let (args, keywords) = self.call_args(lparen)?;
                    self.expect_op(")")?;
                    e = mk(
                        pos,
                        ExprKind::Call {
                            func: Box::new(e),
                            args,
                            keywords,
                        },
                    );
                }
                Tok::Op("[") => {
                    self.advance();
                    let slice = self.slices()?;
                    self.expect_op("]")?;
                    e = mk(
                        pos,
                        ExprKind::Subscript {
                            value: Box::new(e),
                            slice: Box::new(slice),
                            ctx: Ctx::Load,
                        },
                    );
                }
                _ => return Ok(e),
            }
        }
    }

    /// Parses call arguments up to (not including) the closing `)`.
    pub(super) fn call_args(&mut self, lparen: Pos) -> PResult<(Vec<Expr>, Vec<Keyword>)> {
        let mut args: Vec<Expr> = Vec::new();
        let mut keywords: Vec<Keyword> = Vec::new();
        loop {
            if self.at_op(")") {
                break;
            }
            if self.at_op("*") {
                if keywords.iter().any(|k| k.arg.is_none()) {
                    return self
                        .error("iterable argument unpacking follows keyword argument unpacking");
                }
                args.push(self.starred(true)?);
            } else if self.at_op("**") {
                let pos = self.pos();
                self.advance();
                let value = self.expression()?;
                keywords.push(Keyword {
                    pos,
                    arg: None,
                    value,
                });
            } else if matches!(self.tok(), Tok::Name(_)) && matches!(self.peek(1), Tok::Op("=")) {
                let pos = self.pos();
                let name = self.ident()?;
                self.advance();
                let value = self.expression()?;
                keywords.push(Keyword {
                    pos,
                    arg: Some(name),
                    value,
                });
            } else {
                let e = self.named_expression()?;
                if self.at_kw("for")
                    || (self.at_kw("async") && matches!(self.peek(1), Tok::Kw("for")))
                {
                    if !args.is_empty() || !keywords.is_empty() {
                        return self.error("Generator expression must be parenthesized");
                    }
                    let generators = self.comp_for()?;
                    args.push(mk(
                        lparen,
                        ExprKind::GeneratorExp {
                            elt: Box::new(e),
                            generators,
                        },
                    ));
                    if self.at_op(",") {
                        return self.error("Generator expression must be parenthesized");
                    }
                } else {
                    if self.at_op("=") {
                        return self.error(
                            "expression cannot contain assignment, perhaps you meant \"==\"?",
                        );
                    }
                    if let Some(k) = keywords.last() {
                        let msg = if k.arg.is_some() {
                            "positional argument follows keyword argument"
                        } else {
                            "positional argument follows keyword argument unpacking"
                        };
                        return self.error_at(e.pos, msg);
                    }
                    args.push(e);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        Ok((args, keywords))
    }

    fn slices(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.slice_item()?;
        if !self.at_op(",") {
            if matches!(first.kind, ExprKind::Starred { .. }) {
                return Ok(mk(
                    pos,
                    ExprKind::Tuple {
                        elts: vec![first],
                        ctx: Ctx::Load,
                    },
                ));
            }
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.at_op("]") {
                break;
            }
            elts.push(self.slice_item()?);
        }
        Ok(mk(
            pos,
            ExprKind::Tuple {
                elts,
                ctx: Ctx::Load,
            },
        ))
    }

    fn slice_item(&mut self) -> PResult<Expr> {
        if self.at_op("*") {
            return self.starred(false);
        }
        let start = self.pos();
        let walrus = matches!(self.tok(), Tok::Name(_)) && matches!(self.peek(1), Tok::Op(":="));
        let lower = match () {
            _ if self.at_op(":") => None,
            _ if walrus => Some(self.named_expression()?),
            _ => Some(self.expression()?),
        };
        if walrus && self.at_op(":") {
            return Err(self.unexpected());
        }
        if !self.at_op(":") {
            return lower.ok_or_else(|| self.unexpected());
        }
        self.advance();
        let ends = |p: &Parser| p.at_op(":") || p.at_op(",") || p.at_op("]");
        let upper = if ends(self) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut step = None;
        if self.eat_op(":") && !(self.at_op(",") || self.at_op("]")) {
            step = Some(Box::new(self.expression()?));
        }
        Ok(mk(
            start,
            ExprKind::Slice {
                lower: lower.map(Box::new),
                upper,
                step,
            },
        ))
    }

    fn atom(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let kind = match self.tok().clone() {
            Tok::Name(id) => {
                self.advance();
                ExprKind::Name { id, ctx: Ctx::Load }
            }
            Tok::Kw("True") => {
                self.advance();
                ExprKind::Constant(Constant::True)
            }
            Tok::Kw("False") => {
                self.advance();
                ExprKind::Constant(Constant::False)
            }
            Tok::Kw("None") => {
                self.advance();
                ExprKind::Constant(Constant::None)
            }
            Tok::Int(s) => {
                self.advance();
                ExprKind::Constant(Constant::Int(s))
            }
            Tok::Float(f) => {
                self.advance();
                ExprKind::Constant(Constant::Float(f))
            }
            Tok::Imag(f) => {
                self.advance();
                ExprKind::Constant(Constant::Complex(f))
            }
            Tok::Str(_) | Tok::Bytes(_) | Tok::FStr { .. } => return self.strings(),
            Tok::Op("...") => {
                self.advance();
                ExprKind::Constant(Constant::Ellipsis)
            }
            Tok::Op("(") => return self.paren(),
            Tok::Op("[") => return self.list_display(),
            Tok::Op("{") => return self.brace_display(),
            _ => return Err(self.unexpected()),
        };
        Ok(mk(pos, kind))
    }

    fn at_comp_for(&self) -> bool {
        self.at_kw("for") || (self.at_kw("async") && matches!(self.peek(1), Tok::Kw("for")))
    }

    fn paren(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        if self.eat_op(")") {
            return Ok(mk(
                pos,
                ExprKind::Tuple {
                    elts: Vec::new(),
                    ctx: Ctx::Load,
                },
            ));
        }
        if self.at_kw("yield") {
            let y = self.yield_expr()?;
            self.expect_op(")")?;
            return Ok(y);
        }
        let first = self.star_named_expression()?;
        if self.at_comp_for() {
            if matches!(first.kind, ExprKind::Starred { .. }) {
                return self.error_at(
                    first.pos,
                    "iterable unpacking cannot be used in comprehension",
                );
            }
            let generators = self.comp_for()?;
            self.expect_op(")")?;
            return Ok(mk(
                pos,
                ExprKind::GeneratorExp {
                    elt: Box::new(first),
                    generators,
                },
            ));
        }
        if self.at_op(",") {
            let mut elts = vec![first];
            while self.eat_op(",") {
                if self.at_op(")") {
                    break;
                }
                elts.push(self.star_named_expression()?);
            }
            self.expect_op(")")?;
            return Ok(mk(
                pos,
                ExprKind::Tuple {
                    elts,
                    ctx: Ctx::Load,
                },
            ));
        }
        self.expect_op(")")?;
        if matches!(first.kind, ExprKind::Starred { .. }) {
            return self.error_at(first.pos, "cannot use starred expression here");
        }
        Ok(first)
    }

    fn list_display(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        if self.eat_op("]") {
            return Ok(mk(
                pos,
                ExprKind::List {
                    elts: Vec::new(),
                    ctx: Ctx::Load,
                },
            ));
        }
        let first = self.star_named_expression()?;
        if self.at_comp_for() {
            if matches!(first.kind, ExprKind::Starred { .. }) {
                return self.error_at(
                    first.pos,
                    "iterable unpacking cannot be used in comprehension",
                );
            }
            let generators = self.comp_for()?;
            self.expect_op("]")?;
            return Ok(mk(
                pos,
                ExprKind::ListComp {
                    elt: Box::new(first),
                    generators,
                },
            ));
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.at_op("]") {
                break;
            }
            elts.push(self.star_named_expression()?);
        }
        self.expect_op("]")?;
        Ok(mk(
            pos,
            ExprKind::List {
                elts,
                ctx: Ctx::Load,
            },
        ))
    }

    fn brace_display(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        if self.eat_op("}") {
            return Ok(mk(
                pos,
                ExprKind::Dict {
                    keys: Vec::new(),
                    values: Vec::new(),
                },
            ));
        }
        let mut keys: Vec<Option<Expr>> = Vec::new();
        let mut values: Vec<Expr> = Vec::new();
        let mut elts: Vec<Expr> = Vec::new();
        let is_dict;
        if self.at_op("**") {
            self.advance();
            keys.push(None);
            values.push(self.binary(1)?);
            is_dict = true;
        } else {
            let first = self.star_named_expression()?;
            if !matches!(first.kind, ExprKind::Starred { .. }) && self.at_op(":") {
                self.advance();
                let value = self.expression()?;
                if self.at_comp_for() {
                    let generators = self.comp_for()?;
                    self.expect_op("}")?;
                    return Ok(mk(
                        pos,
                        ExprKind::DictComp {
                            key: Box::new(first),
                            value: Box::new(value),
                            generators,
                        },
                    ));
                }
                keys.push(Some(first));
                values.push(value);
                is_dict = true;
            } else {
                if self.at_comp_for() {
                    if matches!(first.kind, ExprKind::Starred { .. }) {
                        return self.error_at(
                            first.pos,
                            "iterable unpacking cannot be used in comprehension",
                        );
                    }
                    let generators = self.comp_for()?;
                    self.expect_op("}")?;
                    return Ok(mk(
                        pos,
                        ExprKind::SetComp {
                            elt: Box::new(first),
                            generators,
                        },
                    ));
                }
                elts.push(first);
                is_dict = false;
            }
        }
        while self.eat_op(",") {
            if self.at_op("}") {
                break;
            }
            if is_dict {
                if self.eat_op("**") {
                    keys.push(None);
                    values.push(self.binary(1)?);
                } else {
                    let key = self.expression()?;
                    self.expect_op(":")?;
                    keys.push(Some(key));
                    values.push(self.expression()?);
                }
            } else {
                elts.push(self.star_named_expression()?);
            }
        }
        self.expect_op("}")?;
        if is_dict {
            Ok(mk(pos, ExprKind::Dict { keys, values }))
        } else {
            Ok(mk(pos, ExprKind::Set(elts)))
        }
    }

    pub(super) fn comp_for(&mut self) -> PResult<Vec<Comprehension>> {
        let mut gens = Vec::new();
        while self.at_comp_for() {
            let is_async = self.eat_kw("async");
            self.advance();
            let target = self.star_targets()?;
            self.expect_kw("in")?;
            let iter = self.disjunction()?;
            let mut ifs = Vec::new();
            while self.eat_kw("if") {
                ifs.push(self.disjunction()?);
            }
            gens.push(Comprehension {
                target,
                iter,
                ifs,
                is_async,
            });
        }
        Ok(gens)
    }

    /// Assignment-target list as used by `for`; the result has `Store` context.
    pub(super) fn star_targets(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let first = self.star_target()?;
        let mut target = if self.at_op(",") {
            let mut elts = vec![first];
            while self.eat_op(",") {
                if !self.can_start_expr() {
                    break;
                }
                elts.push(self.star_target()?);
            }
            mk(
                pos,
                ExprKind::Tuple {
                    elts,
                    ctx: Ctx::Load,
                },
            )
        } else {
            first
        };
        set_ctx(&mut target, Ctx::Store)?;
        Ok(target)
    }

    pub(super) fn star_target(&mut self) -> PResult<Expr> {
        if self.at_op("*") {
            return self.starred(false);
        }
        self.binary(1)
    }

    pub(super) fn yield_expr(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        self.advance();
        if self.eat_kw("from") {
            let value = self.expression()?;
            return Ok(mk(pos, ExprKind::YieldFrom(Box::new(value))));
        }
        let value = if self.can_start_expr() {
            Some(Box::new(self.star_expressions()?))
        } else {
            None
        };
        Ok(mk(pos, ExprKind::Yield(value)))
    }

    /// Parses `def`/`lambda` parameters up to (not including) the closing `)` or `:`.
    pub(super) fn parameters(&mut self, is_lambda: bool) -> PResult<Arguments> {
        let end = if is_lambda { ":" } else { ")" };
        let mut a = Arguments::default();
        let mut plain: Vec<Arg> = Vec::new();
        let (mut seen_star, mut seen_slash, mut seen_default) = (false, false, false);
        loop {
            if self.at_op(end) {
                break;
            }
            if self.at_op("/") {
                if seen_slash {
                    return self.error("/ may appear only once");
                }
                if seen_star {
                    return self.error("/ must be ahead of *");
                }
                if plain.is_empty() {
                    return self.error("at least one argument must precede /");
                }
                self.advance();
                a.posonlyargs = std::mem::take(&mut plain);
                seen_slash = true;
            } else if self.at_op("**") {
                self.advance();
                a.kwarg = Some(self.param(is_lambda, false)?);
                self.eat_op(",");
                if !self.at_op(end) {
                    return self.error("arguments cannot follow var-keyword argument");
                }
                break;
            } else if self.at_op("*") {
                if seen_star {
                    return self.error("* argument may appear only once");
                }
                self.advance();
                seen_star = true;
                if matches!(self.tok(), Tok::Name(_)) {
                    a.vararg = Some(self.param(is_lambda, true)?);
                } else if !self.at_op(",")
                    || matches!(self.peek(1), Tok::Op(")") | Tok::Op(":") | Tok::Op("**"))
                {
                    return self.error("named arguments must follow bare *");
                }
            } else {
                let arg = self.param(is_lambda, false)?;
                let default = if self.eat_op("=") {
                    Some(self.expression()?)
                } else {
                    None
                };
                if seen_star {
                    a.kwonlyargs.push(arg);
                    a.kw_defaults.push(default);
                } else {
                    match default {
                        Some(d) => {
                            a.defaults.push(d);
                            seen_default = true;
                        }
                        None if seen_default => {
                            return self.error_at(
                                arg.pos,
                                "non-default argument follows default argument",
                            );
                        }
                        None => {}
                    }
                    plain.push(arg);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        a.args = plain;
        Ok(a)
    }

    fn param(&mut self, is_lambda: bool, star: bool) -> PResult<Arg> {
        let pos = self.pos();
        let arg = self.ident()?;
        let mut annotation = None;
        if !is_lambda && self.eat_op(":") {
            annotation = Some(if star && self.at_op("*") {
                self.starred(true)?
            } else {
                self.expression()?
            });
        }
        Ok(Arg {
            pos,
            arg,
            annotation,
        })
    }
}
