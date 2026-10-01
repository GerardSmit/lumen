//! String literal concatenation and f-string (PEP 701) parsing.

use std::rc::Rc;

use super::{mk, PResult, Parser, SyntaxError};
use crate::ast::*;
use crate::lexer::string::{decode_str, scan_field_expr, skip_debug_ws, FieldEnd, ScanErr};
use crate::lexer::{self, Tok};

enum Part {
    Lit(String, Pos),
    Value(Expr),
}

struct Body<'a> {
    s: &'a [char],
    raw: bool,
    line: u32,
    col: u32,
    newlines: Vec<usize>,
    tok_pos: Pos,
}

impl Body<'_> {
    fn pos_of(&self, i: usize) -> (u32, u32) {
        let n = self.newlines.partition_point(|&nl| nl < i);
        if n == 0 {
            (self.line, self.col + i as u32)
        } else {
            (self.line + n as u32, (i - self.newlines[n - 1] - 1) as u32)
        }
    }

    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        Err(SyntaxError {
            msg: msg.into(),
            line: self.tok_pos.line,
            col: self.tok_pos.col,
        })
    }

    fn scan_err<T>(&self, e: ScanErr) -> PResult<T> {
        match e {
            ScanErr::Unterminated => self.err("f-string: expecting '}'"),
            ScanErr::Msg(m) => self.err(m),
        }
    }
}

fn join_parts(parts: Vec<Part>) -> Vec<Expr> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_pos = Pos::default();
    let flush = |cur: &mut String, pos: Pos, out: &mut Vec<Expr>| {
        if !cur.is_empty() {
            let s: Rc<str> = Rc::from(cur.as_str());
            out.push(mk(pos, ExprKind::Constant(Constant::Str(s))));
            cur.clear();
        }
    };
    for part in parts {
        match part {
            Part::Lit(s, pos) => {
                if cur.is_empty() {
                    cur_pos = pos;
                }
                cur.push_str(&s);
            }
            Part::Value(e) => {
                flush(&mut cur, cur_pos, &mut out);
                out.push(e);
            }
        }
    }
    flush(&mut cur, cur_pos, &mut out);
    out
}

impl Parser {
    /// Parses one or more adjacent string literals into a single expression.
    pub(super) fn strings(&mut self) -> PResult<Expr> {
        let pos = self.pos();
        let (mut seen_bytes, mut seen_str, mut has_f) = (false, false, false);
        let mut parts: Vec<Part> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let tok_pos = self.pos();
            match self.tok() {
                Tok::Str(s) => {
                    seen_str = true;
                    parts.push(Part::Lit(s.to_string(), tok_pos));
                }
                Tok::Bytes(b) => {
                    seen_bytes = true;
                    bytes.extend_from_slice(b);
                }
                Tok::FStr {
                    body,
                    raw,
                    line,
                    col,
                } => {
                    seen_str = true;
                    has_f = true;
                    let (body, raw, line, col) = (body.clone(), *raw, *line, *col);
                    let b = Body {
                        s: &body,
                        raw,
                        line,
                        col,
                        newlines: body
                            .iter()
                            .enumerate()
                            .filter(|(_, &c)| c == '\n')
                            .map(|(i, _)| i)
                            .collect(),
                        tok_pos,
                    };
                    let (_, p) = self.fstring_walk(&b, 0, false)?;
                    parts.extend(p);
                }
                _ => break,
            }
            if seen_bytes && seen_str {
                return self.error_at(pos, "cannot mix bytes and nonbytes literals");
            }
            self.advance();
        }
        if seen_bytes {
            return Ok(mk(
                pos,
                ExprKind::Constant(Constant::Bytes(Rc::from(bytes))),
            ));
        }
        if has_f {
            return Ok(mk(pos, ExprKind::JoinedStr(join_parts(parts))));
        }
        let mut s = String::new();
        for part in parts {
            if let Part::Lit(l, _) = part {
                s.push_str(&l);
            }
        }
        Ok(mk(
            pos,
            ExprKind::Constant(Constant::Str(Rc::from(s.as_str()))),
        ))
    }

    fn fstring_walk(
        &mut self,
        b: &Body,
        mut i: usize,
        in_spec: bool,
    ) -> PResult<(usize, Vec<Part>)> {
        let s = b.s;
        let mut parts = Vec::new();
        let mut seg: Vec<char> = Vec::new();
        let mut seg_start = 0;
        let flush = |seg: &mut Vec<char>, start: usize, parts: &mut Vec<Part>| -> PResult<()> {
            if seg.is_empty() {
                return Ok(());
            }
            let (line, col) = b.pos_of(start);
            match decode_str(seg, b.raw) {
                Ok(text) => parts.push(Part::Lit(text, Pos { line, col })),
                Err(m) => return b.err(m),
            }
            seg.clear();
            Ok(())
        };
        loop {
            let Some(&c) = s.get(i) else {
                if in_spec {
                    return b.err("f-string: expecting '}'");
                }
                break;
            };
            if seg.is_empty() {
                seg_start = i;
            }
            match c {
                '\\' => match s.get(i + 1) {
                    Some(&n) if n == '{' || n == '}' => {
                        seg.push('\\');
                        i += 1;
                    }
                    Some('N') if !b.raw && s.get(i + 2) == Some(&'{') => {
                        let end = s[i..]
                            .iter()
                            .position(|&c| c == '}')
                            .map_or(s.len(), |p| i + p + 1);
                        seg.extend(&s[i..end]);
                        i = end;
                    }
                    Some(&n) => {
                        seg.push('\\');
                        seg.push(n);
                        i += 2;
                    }
                    None => {
                        seg.push('\\');
                        i += 1;
                    }
                },
                '{' if !in_spec && s.get(i + 1) == Some(&'{') => {
                    seg.push('{');
                    i += 2;
                }
                '{' => {
                    flush(&mut seg, seg_start, &mut parts)?;
                    let (next, field) = self.fstring_field(b, i + 1)?;
                    parts.extend(field);
                    i = next;
                }
                '}' if in_spec => break,
                '}' if s.get(i + 1) == Some(&'}') => {
                    seg.push('}');
                    i += 2;
                }
                '}' => return b.err("f-string: single '}' is not allowed"),
                _ => {
                    seg.push(c);
                    i += 1;
                }
            }
        }
        flush(&mut seg, seg_start, &mut parts)?;
        Ok((i, parts))
    }

    /// Parses a replacement field starting just after its `{`; returns the index after `}`.
    fn fstring_field(&mut self, b: &Body, start: usize) -> PResult<(usize, Vec<Part>)> {
        let s = b.s;
        let (end, kind) = match scan_field_expr(s, start, self.depth) {
            Ok(r) => r,
            Err(e) => return b.scan_err(e),
        };
        let text = &s[start..end];
        if text.iter().all(|c| c.is_whitespace()) {
            return b.err("f-string: valid expression required before '}'");
        }
        let (line, col) = b.pos_of(start);
        let brace = b.pos_of(start - 1);
        let brace = Pos {
            line: brace.0,
            col: brace.1,
        };
        let value = self.fstring_expr(text, line, col, b)?;
        let mut parts = Vec::new();
        let mut j = end;
        let debug = kind == FieldEnd::Debug;
        if debug {
            let after_eq = end + 1;
            j = skip_debug_ws(s, end);
            let mut text: String = s[start..after_eq].iter().collect();
            let mut in_comment = false;
            for &c in &s[after_eq..j] {
                in_comment = (in_comment || c == '#') && c != '\n';
                if !in_comment {
                    text.push(c);
                }
            }
            parts.push(Part::Lit(text, Pos { line, col }));
        }
        let mut conversion = None;
        if s.get(j) == Some(&'!') {
            j += 1;
            match s.get(j) {
                Some(&c @ ('s' | 'r' | 'a')) => conversion = Some(c),
                _ => {
                    return b
                        .err("f-string: invalid conversion character: expected 's', 'r', or 'a'")
                }
            }
            j += 1;
            while s.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
        }
        let mut format_spec = None;
        if s.get(j) == Some(&':') {
            let (k, spec) = self.fstring_walk(b, j + 1, true)?;
            let (cl, cc) = b.pos_of(j);
            let colon = Pos { line: cl, col: cc };
            format_spec = Some(Box::new(mk(colon, ExprKind::JoinedStr(join_parts(spec)))));
            j = k;
        }
        if s.get(j) != Some(&'}') {
            return b.err("f-string: expecting '}'");
        }
        if debug && conversion.is_none() && format_spec.is_none() {
            conversion = Some('r');
        }
        parts.push(Part::Value(mk(
            brace,
            ExprKind::FormattedValue {
                value: Box::new(value),
                conversion,
                format_spec,
            },
        )));
        Ok((j + 1, parts))
    }

    fn fstring_expr(&mut self, text: &[char], line: u32, col: u32, b: &Body) -> PResult<Expr> {
        let toks = lexer::tokenize_fragment(text, line, col)?;
        let mut sub = Parser::new(toks, self.depth);
        let e = if sub.at_kw("yield") {
            sub.yield_expr()?
        } else {
            sub.star_expressions()?
        };
        if !matches!(sub.tok(), Tok::EndMarker) {
            return b.err("f-string: invalid syntax");
        }
        Ok(e)
    }
}
