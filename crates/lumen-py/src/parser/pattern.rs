//! `match` statement patterns.

use super::{mk, PResult, Parser};
use crate::ast::*;
use crate::lexer::Tok;

impl Parser {
    /// `patterns`: an open sequence pattern or a single pattern.
    pub(super) fn patterns(&mut self) -> PResult<Pattern> {
        let first = self.maybe_star_pattern()?;
        if !self.at_op(",") {
            if matches!(first, Pattern::MatchStar(_)) {
                return Err(self.unexpected());
            }
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_op(",") {
            if self.at_op(":") || self.at_kw("if") {
                break;
            }
            items.push(self.maybe_star_pattern()?);
        }
        Ok(Pattern::MatchSequence(items))
    }

    fn maybe_star_pattern(&mut self) -> PResult<Pattern> {
        if self.eat_op("*") {
            let name = self.ident()?;
            let name = if &*name == "_" { None } else { Some(name) };
            return Ok(Pattern::MatchStar(name));
        }
        self.pattern()
    }

    fn pattern(&mut self) -> PResult<Pattern> {
        self.enter()?;
        let r = self.pattern_inner();
        self.leave();
        r
    }

    fn pattern_inner(&mut self) -> PResult<Pattern> {
        let first = self.closed_pattern()?;
        let pat = if self.at_op("|") {
            let mut alts = vec![first];
            while self.eat_op("|") {
                alts.push(self.closed_pattern()?);
            }
            Pattern::MatchOr(alts)
        } else {
            first
        };
        if self.eat_kw("as") {
            let pos = self.pos();
            let name = self.ident()?;
            if &*name == "_" {
                return self.error_at(pos, "cannot use '_' as a target");
            }
            return Ok(Pattern::MatchAs {
                pattern: Some(Box::new(pat)),
                name: Some(name),
            });
        }
        Ok(pat)
    }

    fn closed_pattern(&mut self) -> PResult<Pattern> {
        match self.tok() {
            Tok::Int(_) | Tok::Float(_) | Tok::Imag(_) | Tok::Op("-") => {
                Ok(Pattern::MatchValue(self.signed_number()?))
            }
            Tok::Str(_) | Tok::Bytes(_) | Tok::FStr { .. } => {
                let pos = self.pos();
                let e = self.strings()?;
                if let ExprKind::JoinedStr(values) = &e.kind {
                    if values
                        .iter()
                        .any(|v| matches!(v.kind, ExprKind::FormattedValue { .. }))
                    {
                        return self.error_at(
                            pos,
                            "patterns may only match literals and attribute lookups",
                        );
                    }
                }
                Ok(Pattern::MatchValue(e))
            }
            Tok::Kw("None") => {
                self.advance();
                Ok(Pattern::MatchSingleton(Constant::None))
            }
            Tok::Kw("True") => {
                self.advance();
                Ok(Pattern::MatchSingleton(Constant::True))
            }
            Tok::Kw("False") => {
                self.advance();
                Ok(Pattern::MatchSingleton(Constant::False))
            }
            Tok::Name(_) => self.name_pattern(),
            Tok::Op("(") => self.group_pattern(),
            Tok::Op("[") => {
                self.advance();
                let items = self.pattern_list("]")?;
                Ok(Pattern::MatchSequence(items))
            }
            Tok::Op("{") => self.mapping_pattern(),
            _ => Err(self.unexpected()),
        }
    }

    fn signed_number(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let neg = self.eat_op("-");
        let lit = |p: &mut Parser| -> PResult<Expr> {
            let pos = p.pos();
            let c = match p.tok() {
                Tok::Int(s) => Constant::Int(s.clone()),
                Tok::Float(f) => Constant::Float(*f),
                Tok::Imag(f) => Constant::Complex(*f),
                _ => return Err(p.unexpected()),
            };
            p.advance();
            Ok(mk(pos, ExprKind::Constant(c)))
        };
        let mut e = lit(self)?;
        if neg {
            e = mk(
                pos,
                ExprKind::UnaryOp {
                    op: UnaryOp::USub,
                    operand: Box::new(e),
                },
            );
        }
        if self.at_op("+") || self.at_op("-") {
            let op = if self.at_op("+") {
                BinOp::Add
            } else {
                BinOp::Sub
            };
            self.advance();
            if !matches!(self.tok(), Tok::Imag(_)) {
                return self.error("imaginary number required in complex literal");
            }
            let right = lit(self)?;
            e = mk(
                pos,
                ExprKind::BinOp {
                    left: Box::new(e),
                    op,
                    right: Box::new(right),
                },
            );
        }
        Ok(e)
    }

    fn name_pattern(&mut self) -> PResult<Pattern> {
        let pos = self.pos();
        let first = self.ident()?;
        if !self.at_op(".") && !self.at_op("(") {
            let name = if &*first == "_" { None } else { Some(first) };
            return Ok(Pattern::MatchAs {
                pattern: None,
                name,
            });
        }
        let mut e = mk(
            pos,
            ExprKind::Name {
                id: first,
                ctx: Ctx::Load,
            },
        );
        while self.eat_op(".") {
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
        if !self.at_op("(") {
            return Ok(Pattern::MatchValue(e));
        }
        self.advance();
        let mut patterns = Vec::new();
        let mut kwd_attrs = Vec::new();
        let mut kwd_patterns = Vec::new();
        loop {
            if self.at_op(")") {
                break;
            }
            if matches!(self.tok(), Tok::Name(_)) && matches!(self.peek(1), Tok::Op("=")) {
                kwd_attrs.push(self.ident()?);
                self.advance();
                kwd_patterns.push(self.pattern()?);
            } else {
                if !kwd_attrs.is_empty() {
                    return self.error("positional patterns follow keyword patterns");
                }
                patterns.push(self.pattern()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Pattern::MatchClass {
            cls: e,
            patterns,
            kwd_attrs,
            kwd_patterns,
        })
    }

    fn group_pattern(&mut self) -> PResult<Pattern> {
        self.advance();
        if self.eat_op(")") {
            return Ok(Pattern::MatchSequence(Vec::new()));
        }
        let first = self.maybe_star_pattern()?;
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op(")") {
                    break;
                }
                items.push(self.maybe_star_pattern()?);
            }
            self.expect_op(")")?;
            return Ok(Pattern::MatchSequence(items));
        }
        self.expect_op(")")?;
        if matches!(first, Pattern::MatchStar(_)) {
            return Ok(Pattern::MatchSequence(vec![first]));
        }
        Ok(first)
    }

    fn pattern_list(&mut self, close: &str) -> PResult<Vec<Pattern>> {
        let mut items = Vec::new();
        loop {
            if self.at_op(close) {
                break;
            }
            items.push(self.maybe_star_pattern()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(close)?;
        Ok(items)
    }

    fn mapping_key(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        match self.tok() {
            Tok::Int(_) | Tok::Float(_) | Tok::Imag(_) | Tok::Op("-") => self.signed_number(),
            Tok::Str(_) | Tok::Bytes(_) => self.strings(),
            Tok::Kw("None") => {
                self.advance();
                Ok(mk(pos, ExprKind::Constant(Constant::None)))
            }
            Tok::Kw("True") => {
                self.advance();
                Ok(mk(pos, ExprKind::Constant(Constant::True)))
            }
            Tok::Kw("False") => {
                self.advance();
                Ok(mk(pos, ExprKind::Constant(Constant::False)))
            }
            Tok::Name(_) => {
                let first = self.ident()?;
                if !self.at_op(".") {
                    return self.error_at(
                        pos,
                        "mapping pattern keys may only match literals and attribute lookups",
                    );
                }
                let mut e = mk(
                    pos,
                    ExprKind::Name {
                        id: first,
                        ctx: Ctx::Load,
                    },
                );
                while self.eat_op(".") {
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
                Ok(e)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn mapping_pattern(&mut self) -> PResult<Pattern> {
        self.advance();
        let mut keys = Vec::new();
        let mut patterns = Vec::new();
        let mut rest = None;
        loop {
            if self.at_op("}") {
                break;
            }
            if self.eat_op("**") {
                rest = Some(self.ident()?);
                self.eat_op(",");
                break;
            }
            keys.push(self.mapping_key()?);
            self.expect_op(":")?;
            patterns.push(self.pattern()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op("}")?;
        Ok(Pattern::MatchMapping {
            keys,
            patterns,
            rest,
        })
    }
}
