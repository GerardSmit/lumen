//! Array destructuring the batched `Op::DestructureArr` walk cannot express: nested patterns,
//! defaults, rest elements and captured leaves. The iterator is stepped one element at a time,
//! each element bound (default evaluated, nested pattern destructured) before the next step —
//! the spec's interleaving.
//!
//! Hidden slots hold the iterator, its `next` and a *done* flag. The flag is set before every
//! step (a step's own throw leaves the iterator unclosed) and cleared when the step yields.
//! The element code runs under a handler whose pad closes the iterator in throw mode unless it
//! is done; a normal completion closes a not-done iterator in normal mode.
use super::{Bail, CResult, Compiler, Op};
use crate::ast::{ArrayPatElem, DeclKind, Pattern};
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;

/// `IterRestL(it, next)`: the remaining values of the iterator, as a fresh array.
pub(super) fn rest(i: &mut Interp, slots: &[Value], it: u16, nx: u16) -> Result<Value, Abrupt> {
    // An encoded protocol-free state drains natively (see `iter_fast::drain`).
    let out = super::iter_fast::drain(i, &slots[it as usize].clone(), &slots[nx as usize].clone())?;
    Ok(i.make_array(out))
}

fn has_member_target(p: &Pattern) -> bool {
    match p {
        Pattern::Ident(_) => false,
        Pattern::Member(_) => true,
        Pattern::Object(o) => o.props.iter().any(|q| has_member_target(&q.value)),
        Pattern::Array(elems) => elems.iter().any(|e| match e {
            ArrayPatElem::Hole => false,
            ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                has_member_target(pattern)
            }
        }),
    }
}

/// The hidden slots and constants of one sequential walk.
struct Walk {
    it: u16,
    nx: u16,
    done: u16,
    k_true: u32,
}

impl Compiler {
    /// Sequential array destructuring of the value on the stack (see the module docs).
    pub(super) fn destructure_array_seq(&mut self, elems: &[ArrayPatElem], kind: DeclKind) -> CResult {
        // A destructuring *assignment* evaluates a member target's reference before the step;
        // this lowering binds after it — leave those to the tree-walker.
        if self.assign_mode
            && elems.iter().any(|e| match e {
                ArrayPatElem::Hole => false,
                ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                    has_member_target(pattern)
                }
            })
        {
            return Err(Bail);
        }
        // A generator's `return()` resumption at a `yield` inside a default would leave this
        // region without the iterator close the spec performs.
        if self.generator
            && elems
                .iter()
                .any(|e| matches!(e, ArrayPatElem::Elem { default: Some(_), .. }))
        {
            return Err(Bail);
        }
        let w = Walk {
            it: self.fresh_slot("%dseq_iter%"),
            nx: self.fresh_slot("%dseq_next%"),
            done: self.fresh_slot("%dseq_done%"),
            k_true: self.const_idx(Value::Bool(true)),
        };
        let k_false = self.const_idx(Value::Bool(false));
        self.emit(Op::GetIter);
        self.emit(Op::StoreLocal(w.nx));
        self.emit(Op::StoreLocal(w.it));
        self.emit(Op::Const(k_false));
        self.emit(Op::StoreLocal(w.done));
        let push = self.emit(Op::PushHandler(0));
        self.try_depth += 1;
        let r = self.destructure_array_elems(elems, kind, &w);
        self.try_depth -= 1;
        r?;
        self.emit(Op::PopHandler);
        // Normal completion: close a not-done iterator (close errors propagate).
        self.emit(Op::LoadLocal(w.done));
        let j_close = self.emit(Op::JumpIfFalse(0));
        let j_end = self.emit(Op::Jump(0));
        self.patch(j_close);
        self.emit(Op::IterCloseL(w.it));
        let j_end2 = self.emit(Op::Jump(0));
        // Abrupt element: close in throw mode unless done, rethrow.
        let pad = self.ops.len() as u32;
        if let Op::PushHandler(t) = &mut self.ops[push] {
            *t = pad;
        }
        self.emit(Op::LoadLocal(w.done));
        let j_abort = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::Throw);
        self.patch(j_abort);
        self.emit(Op::IterAbortL(w.it));
        self.patch(j_end);
        self.patch(j_end2);
        Ok(())
    }

    fn destructure_array_elems(&mut self, elems: &[ArrayPatElem], kind: DeclKind, w: &Walk) -> CResult {
        for e in elems {
            match e {
                ArrayPatElem::Hole | ArrayPatElem::Elem { .. } => {
                    // [value]: the next element, or undefined once done.
                    self.emit(Op::LoadLocal(w.done));
                    let j_step = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::Undef);
                    let j_after = self.emit(Op::Jump(0));
                    self.patch(j_step);
                    self.emit(Op::Const(w.k_true));
                    self.emit(Op::StoreLocal(w.done));
                    self.emit(Op::IterStepL(w.it, w.nx));
                    self.emit(Op::Not);
                    self.emit(Op::StoreLocal(w.done));
                    self.patch(j_after);
                    match e {
                        ArrayPatElem::Elem { pattern, default } => {
                            if let Some(d) = default {
                                self.emit(Op::Dup);
                                self.emit(Op::Undef);
                                self.emit(Op::StrictEq);
                                let skip = self.emit(Op::JumpIfFalse(0));
                                self.emit(Op::Pop);
                                match pattern {
                                    Pattern::Ident(n) => self.named_expr(d, n)?,
                                    _ => self.expr(d)?,
                                }
                                self.patch(skip);
                            }
                            self.destructure_store(pattern, kind)?;
                        }
                        _ => {
                            self.emit(Op::Pop);
                        }
                    }
                }
                ArrayPatElem::Rest(pattern) => {
                    self.emit(Op::LoadLocal(w.done));
                    let j_collect = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::MakeArray(0));
                    let j_after = self.emit(Op::Jump(0));
                    self.patch(j_collect);
                    self.emit(Op::Const(w.k_true));
                    self.emit(Op::StoreLocal(w.done));
                    self.emit(Op::IterRestL(w.it, w.nx));
                    self.patch(j_after);
                    self.destructure_store(pattern, kind)?;
                }
            }
        }
        Ok(())
    }
}
