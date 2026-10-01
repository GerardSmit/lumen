//! Lowering of the [`Node`] tree to a flat [`Inst`] program.

use super::ir::Node;
use super::program::{Dialect, Inst, Rep};
use std::rc::Rc;

const MAX_REPEAT: usize = 1000;
/// Python permits counted repeats of any body up to 2^32-2; the body is unrolled, so cap the
/// count and the whole program instead.
const MAX_REPEAT_PY: usize = 20_000;
const MAX_PROGRAM: usize = 4_000_000;
pub(crate) const NEST_ERROR: &str = "regular expression too deeply nested";

pub(super) fn compile_program(
    node: &Node,
    ngroups: usize,
    dialect: Dialect,
) -> Result<(Vec<Inst>, usize), String> {
    let mut c = Compiler {
        nmarks: 0,
        ngroups,
        dialect,
    };
    // Wrap the whole match in group-0 saves.
    let mut prog = vec![Inst::Save(0)];
    c.compile(node, &mut prog)?;
    prog.push(Inst::Save(1));
    prog.push(Inst::Match);
    Ok((prog, c.nmarks))
}

struct Compiler {
    /// Mark ids are globally unique across the whole pattern (nested sub-programs included).
    nmarks: usize,
    ngroups: usize,
    dialect: Dialect,
}

impl Compiler {
    fn compile(&mut self, node: &Node, prog: &mut Vec<Inst>) -> Result<(), String> {
        if crate::stack::exhausted() {
            return Err(NEST_ERROR.into());
        }
        match node {
            Node::Empty => {}
            Node::Char(c) => prog.push(Inst::Char(*c)),
            Node::Any => prog.push(Inst::Any),
            Node::Class(cc) => prog.push(Inst::Class(Rc::new(cc.duplicate()))),
            Node::Start => prog.push(Inst::AssertStart),
            Node::End => prog.push(Inst::AssertEnd),
            Node::StartText => prog.push(Inst::AssertStartText),
            Node::EndText => prog.push(Inst::AssertEndText),
            Node::StartLine => prog.push(Inst::AssertStartLine),
            Node::EndLine => prog.push(Inst::AssertEndLine),
            Node::BackrefMapped(n, pre) => prog.push(Inst::BackrefMapped(*n, *pre)),
            Node::WordB(b, flavor) => prog.push(Inst::WordBoundary(*b, *flavor)),
            Node::Backref(n) => prog.push(Inst::Backref(*n)),
            Node::BackrefAlt(v) => prog.push(Inst::BackrefAlt(Rc::new(v.clone()))),
            // Front ends resolve names to group indices before compiling; a stray one never matches.
            Node::NamedBackref(_) => prog.push(Inst::Backref(0)),
            Node::Modifier { add, remove, inner } => {
                let opt = |a: bool, r: bool| {
                    if a {
                        Some(true)
                    } else if r {
                        Some(false)
                    } else {
                        None
                    }
                };
                prog.push(Inst::PushFlags(
                    opt(add.0, remove.0),
                    opt(add.1, remove.1),
                    opt(add.2, remove.2),
                ));
                self.compile(inner, prog)?;
                prog.push(Inst::PopFlags);
            }
            Node::Concat(v) => {
                for n in v {
                    self.compile(n, prog)?;
                }
            }
            Node::Alt(v) => {
                let mut jmp_ends = Vec::new();
                for (i, alt) in v.iter().enumerate() {
                    if i < v.len() - 1 {
                        let sp = prog.len();
                        prog.push(Inst::Split(0, 0));
                        let a_start = prog.len();
                        self.compile(alt, prog)?;
                        jmp_ends.push(prog.len());
                        prog.push(Inst::Jmp(0));
                        let next = prog.len();
                        prog[sp] = Inst::Split(a_start, next);
                    } else {
                        self.compile(alt, prog)?;
                    }
                }
                let end = prog.len();
                for j in jmp_ends {
                    prog[j] = Inst::Jmp(end);
                }
            }
            Node::Group(idx, inner) => {
                if let Some(i) = idx {
                    prog.push(Inst::Save(2 * i));
                }
                self.compile(inner, prog)?;
                if let Some(i) = idx {
                    prog.push(Inst::Save(2 * i + 1));
                    if self.dialect == Dialect::Python {
                        prog.push(Inst::SetLast(*i));
                    }
                }
            }
            Node::Look(negate, inner) => {
                let sub = self.sub_program(inner)?;
                prog.push(Inst::Look {
                    negate: *negate,
                    prog: sub,
                });
            }
            Node::LookBehind(negate, inner) => {
                // The body is compiled from the REVERSED tree and executed right-to-left.
                let sub = self.sub_program(&reverse_node(inner))?;
                prog.push(Inst::LookBehind {
                    negate: *negate,
                    prog: sub,
                });
            }
            Node::LookBehindFixed {
                negate,
                width,
                body,
            } => {
                let sub = self.sub_program(body)?;
                prog.push(Inst::LookBack {
                    negate: *negate,
                    width: *width,
                    prog: sub,
                });
            }
            Node::Atomic(inner) => {
                let sub = self.sub_program(inner)?;
                prog.push(Inst::Atomic(sub));
            }
            Node::Cond { group, yes, no } => {
                if *group == 0 || *group > self.ngroups {
                    return Err(format!("invalid group reference {group}"));
                }
                let test = prog.len();
                prog.push(Inst::CondGroup(*group, 0));
                self.compile(yes, prog)?;
                let skip = prog.len();
                prog.push(Inst::Jmp(0));
                let otherwise = prog.len();
                self.compile(no, prog)?;
                let end = prog.len();
                prog[test] = Inst::CondGroup(*group, otherwise);
                prog[skip] = Inst::Jmp(end);
            }
            Node::Repeat(inner, min, max, greedy) => {
                self.compile_repeat(inner, *min, *max, *greedy, prog)?
            }
        }
        Ok(())
    }

    fn sub_program(&mut self, node: &Node) -> Result<Rc<Vec<Inst>>, String> {
        let mut sub = Vec::new();
        self.compile(node, &mut sub)?;
        sub.push(Inst::Match);
        Ok(Rc::new(sub))
    }

    fn next_mark(&mut self) -> usize {
        let id = self.nmarks;
        self.nmarks += 1;
        id
    }

    fn compile_repeat(
        &mut self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        prog: &mut Vec<Inst>,
    ) -> Result<(), String> {
        // Fast path: a repeated single-character atom consumes iteratively (no per-character
        // recursion), so arbitrarily large counts (up to 2^53-1) cost nothing to compile.
        if let Some(rep) = single_char_rep(inner) {
            prog.push(Inst::Many {
                rep,
                min,
                max,
                greedy,
            });
            return Ok(());
        }
        // The general path unrolls `min` copies, so bound it to keep compiled programs small.
        let limit = match self.dialect {
            Dialect::Js => MAX_REPEAT,
            Dialect::Python => MAX_REPEAT_PY,
        };
        if min > limit || max.map(|m| m > limit).unwrap_or(false) {
            return Err("repetition count too large".into());
        }
        // ECMAScript's RepeatMatcher clears the captures inside the atom at the start of every
        // iteration; Python keeps them.
        let span = match self.dialect {
            Dialect::Js => cap_span(inner),
            Dialect::Python => None,
        };
        for _ in 0..min {
            self.repeat_body(inner, span, prog)?;
        }
        // Optional iterations enforce the empty-iteration rule: in ECMAScript an iteration that
        // consumes nothing fails (backtracking into the body or out of the loop); in Python it
        // succeeds and leaves the loop.
        let dialect = self.dialect;
        let progress_check = |id: usize| match dialect {
            Dialect::Js => Inst::CheckProgress(id),
            Dialect::Python => Inst::ExitIfEmpty(id, 0),
        };
        let mut exits = Vec::new();
        let mut splits = Vec::new();
        let l1 = prog.len();
        match max {
            None => {
                // Greedy: L1: Split(body, end); body; Jmp(L1); end.
                let id = self.next_mark();
                splits.push((prog.len(), prog.len() + 1));
                prog.push(Inst::Split(0, 0));
                prog.push(Inst::SetMark(id));
                self.repeat_body(inner, span, prog)?;
                exits.push(prog.len());
                prog.push(progress_check(id));
                prog.push(Inst::Jmp(l1));
            }
            Some(m) => {
                for _ in 0..m.saturating_sub(min) {
                    let id = self.next_mark();
                    splits.push((prog.len(), prog.len() + 1));
                    prog.push(Inst::Split(0, 0));
                    prog.push(Inst::SetMark(id));
                    self.repeat_body(inner, span, prog)?;
                    exits.push(prog.len());
                    prog.push(progress_check(id));
                }
            }
        }
        let end = prog.len();
        for (sp, body) in splits {
            prog[sp] = if greedy {
                Inst::Split(body, end)
            } else {
                Inst::Split(end, body)
            };
        }
        for at in exits {
            if let Inst::ExitIfEmpty(id, _) = prog[at] {
                prog[at] = Inst::ExitIfEmpty(id, end);
            }
        }
        Ok(())
    }

    fn repeat_body(
        &mut self,
        inner: &Node,
        span: Option<(usize, usize)>,
        prog: &mut Vec<Inst>,
    ) -> Result<(), String> {
        if prog.len() > MAX_PROGRAM {
            return Err("regular expression too large".into());
        }
        if let Some((lo, hi)) = span {
            prog.push(Inst::ClearCaps(lo, hi));
        }
        self.compile(inner, prog)
    }
}

/// The tree with every concatenation reversed, so a forward compile of the result consumed
/// right-to-left implements backwards matching. Alternative ORDER is preserved; nested
/// lookarounds keep their own orientation (their compile handles direction independently).
fn reverse_node(node: &Node) -> Node {
    match node {
        Node::Concat(v) => Node::Concat(v.iter().rev().map(reverse_node).collect()),
        Node::Alt(v) => Node::Alt(v.iter().map(reverse_node).collect()),
        Node::Group(idx, inner) => Node::Group(*idx, Box::new(reverse_node(inner))),
        Node::Repeat(inner, min, max, greedy) => {
            Node::Repeat(Box::new(reverse_node(inner)), *min, *max, *greedy)
        }
        Node::Atomic(inner) => Node::Atomic(Box::new(reverse_node(inner))),
        Node::Cond { group, yes, no } => Node::Cond {
            group: *group,
            yes: Box::new(reverse_node(yes)),
            no: Box::new(reverse_node(no)),
        },
        Node::Modifier { add, remove, inner } => Node::Modifier {
            add: *add,
            remove: *remove,
            inner: Box::new(reverse_node(inner)),
        },
        other => other.clone(),
    }
}

/// The min/max capture-group indices inside `node`, if any (for per-iteration capture resets).
fn cap_span(node: &Node) -> Option<(usize, usize)> {
    let merge = |a: Option<(usize, usize)>, b: Option<(usize, usize)>| match (a, b) {
        (Some((l1, h1)), Some((l2, h2))) => Some((l1.min(l2), h1.max(h2))),
        (x, None) | (None, x) => x,
    };
    match node {
        Node::Group(idx, inner) => merge(idx.map(|i| (i, i)), cap_span(inner)),
        Node::Concat(v) | Node::Alt(v) => v.iter().fold(None, |acc, n| merge(acc, cap_span(n))),
        Node::Cond { yes, no, .. } => merge(cap_span(yes), cap_span(no)),
        Node::Repeat(inner, ..)
        | Node::Look(_, inner)
        | Node::LookBehind(_, inner)
        | Node::LookBehindFixed { body: inner, .. }
        | Node::Atomic(inner)
        | Node::Modifier { inner, .. } => cap_span(inner),
        _ => None,
    }
}

/// If `node` matches exactly one code point, return it as a `Rep` (for the `Inst::Many` fast path).
fn single_char_rep(node: &Node) -> Option<Rep> {
    match node {
        Node::Char(c) => Some(Rep::Char(*c)),
        Node::Any => Some(Rep::Any),
        Node::Class(cc) => Some(Rep::Class(Rc::new(cc.duplicate()))),
        _ => None,
    }
}
