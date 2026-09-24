//! `try … finally` lowering and the exits (`break`/`continue`/`return`) that cross one.
//!
//! A `finally` region is compiled once. Every way into it records a *route* in a hidden
//! slot (`kind_s`), with the pending value (a thrown exception or a return value) in a second
//! hidden slot (`val_s`):
//!
//! * 0: normal completion of the protected block (or of its `catch`);
//! * 1: a throw, caught by the region's own handler pad;
//! * 2: a `return` crossing the region;
//! * 3 and up: a `break`/`continue` crossing the region, one route per distinct target.
//!
//! After the finally block, a dispatch re-issues the recorded completion from the context
//! *outside* the region, so an exit crossing several nested `finally`s chains through each of
//! them innermost-first. When the finally block itself completes abruptly, the pending completion
//! is discarded, as the spec requires (its jump simply never reaches the dispatch).
use super::{Bail, CResult, Compiler, Op, Pattern};
use crate::ast::Stmt;
use crate::value::Value;

/// Where a crossing jump goes once the finally block completes normally.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Exit {
    Break(usize),
    Continue(usize),
    Return,
}

pub(super) struct FinallyCtx {
    kind_s: u16,
    val_s: u16,
    /// `Compiler::try_depth` outside the region (its handler not yet pushed).
    entry_try_depth: u32,
    /// `Compiler::loops.len()` at region entry: loops at or past this index are inside it.
    loops_len: usize,
    /// Route ids ≥ 3 handed out, with their targets.
    routes: Vec<(u32, Exit)>,
    /// Jumps to the finally block awaiting its address.
    fin_jumps: Vec<usize>,
    /// Some `return` crosses the region (route 2 needs a dispatch arm).
    has_return: bool,
}

impl Compiler {
    fn emit_const_num(&mut self, n: u32) {
        let ci = self.const_idx(Value::Num(n as f64));
        self.emit(Op::Const(ci));
    }

    /// Record route `route` for the innermost finally region and jump into it: pops the handler
    /// regions opened since it was entered (its own included).
    fn jump_into_finally(&mut self, route: u32) {
        let f = self.finallys.last().expect("inside a finally region");
        let (kind_s, floor) = (f.kind_s, f.entry_try_depth);
        self.emit_const_num(route);
        self.emit(Op::StoreLocal(kind_s));
        for _ in floor..self.try_depth {
            self.emit(Op::PopHandler);
        }
        let j = self.emit(Op::Jump(0));
        self.finallys.last_mut().expect("checked").fin_jumps.push(j);
    }

    /// Whether a compiled for-of loop lies inside the innermost finally region (an exit
    /// crossing it would have to close its iterator before running the finally block).
    fn for_of_inside_finally(&self) -> bool {
        let floor = self.finallys.last().map_or(0, |f| f.loops_len);
        self.loops[floor..].iter().any(|c| c.foreach_iter.is_some())
    }

    /// `break`/`continue` targeting `self.loops[idx]`.
    pub(super) fn emit_jump_exit(&mut self, idx: usize, is_continue: bool) -> CResult {
        let crosses = self.finallys.last().is_some_and(|f| f.loops_len > idx);
        if !crosses {
            self.emit_exit_cleanup(idx, is_continue)?;
            let j = self.emit(Op::Jump(0));
            if is_continue {
                self.loops[idx].continues.push(j);
            } else {
                self.loops[idx].breaks.push(j);
            }
            return Ok(());
        }
        if self.for_of_inside_finally() {
            return Err(Bail);
        }
        let exit = if is_continue {
            Exit::Continue(idx)
        } else {
            Exit::Break(idx)
        };
        let f = self.finallys.last_mut().expect("crosses");
        let route = match f.routes.iter().find(|(_, e)| *e == exit) {
            Some(&(r, _)) => r,
            None => {
                let r = 3 + f.routes.len() as u32;
                f.routes.push((r, exit));
                r
            }
        };
        self.jump_into_finally(route);
        Ok(())
    }

    /// Complete a `return` whose value is on the stack.
    pub(super) fn emit_return_tail(&mut self) -> CResult {
        if let Some(f) = self.finallys.last() {
            if self.for_of_inside_finally() {
                return Err(Bail);
            }
            let val_s = f.val_s;
            self.emit(Op::StoreLocal(val_s));
            self.finallys.last_mut().expect("checked").has_return = true;
            self.jump_into_finally(2);
            return Ok(());
        }
        // A return crossing compiled for-of loops must IteratorClose them (the value evaluated
        // first, spec order). One level is modeled exactly; more would need the spec's cascading
        // throw-mode closes — bail to the tree-walker.
        let fors: Vec<(u16, u32)> = self
            .loops
            .iter()
            .filter_map(|c| c.foreach_iter.map(|it| (it, c.body_try_depth)))
            .collect();
        if fors.len() > 1 {
            return Err(Bail);
        }
        if let Some(&(iter_s, body_depth)) = fors.first() {
            // Pop the regions inside the loop body, its handler, then close — a close error
            // propagates to handlers *outside* the loop, replacing the return.
            for _ in body_depth..self.try_depth {
                self.emit(Op::PopHandler);
            }
            self.emit(Op::PopHandler);
            self.emit(Op::IterCloseL(iter_s));
        }
        self.emit(if self.derived {
            Op::DerivedReturn
        } else {
            Op::Return
        });
        Ok(())
    }

    /// `try { block } catch (param) { handler }` (no finally).
    pub(super) fn try_catch(
        &mut self,
        block: &[Stmt],
        handler: Option<&(Option<Pattern>, Vec<Stmt>)>,
    ) -> CResult {
        let Some((param, catch_body)) = handler else {
            return Err(Bail);
        };
        if matches!(param, Some(p) if !matches!(p, Pattern::Ident(_))) {
            return Err(Bail); // destructuring catch param
        }
        let push = self.emit(Op::PushHandler(0));
        self.try_depth += 1;
        self.scopes.push(Vec::new());
        let tr = self.block_body(block);
        self.scopes.pop();
        tr?;
        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        let jmp_after = self.emit(Op::Jump(0));
        // Catch entry: the exception is on the stack.
        let catch_pc = self.ops.len() as u32;
        match &mut self.ops[push] {
            Op::PushHandler(t) => *t = catch_pc,
            _ => unreachable!(),
        }
        self.scopes.push(Vec::new());
        match param {
            Some(Pattern::Ident(name)) => {
                let slot = self.fresh_slot(name);
                self.scope_bind(name, slot, false);
                self.emit(Op::StoreLocal(slot));
            }
            _ => {
                self.emit(Op::Pop); // no binding (or `catch {}`): discard the exception
            }
        }
        let cr = self.block_body(catch_body);
        self.scopes.pop();
        cr?;
        self.patch(jmp_after);
        Ok(())
    }

    /// `try { block } [catch …] finally { fin }`.
    pub(super) fn try_finally(
        &mut self,
        block: &[Stmt],
        handler: Option<&(Option<Pattern>, Vec<Stmt>)>,
        fin: &[Stmt],
    ) -> CResult {
        let kind_s = self.fresh_slot("%fin_kind%");
        let val_s = self.fresh_slot("%fin_val%");
        let entry_try_depth = self.try_depth;
        let push = self.emit(Op::PushHandler(0));
        self.try_depth += 1;
        self.finallys.push(FinallyCtx {
            kind_s,
            val_s,
            entry_try_depth,
            loops_len: self.loops.len(),
            routes: Vec::new(),
            fin_jumps: Vec::new(),
            has_return: false,
        });
        let r = if handler.is_some() {
            self.try_catch(block, handler)
        } else {
            self.scopes.push(Vec::new());
            let r = self.block_body(block);
            self.scopes.pop();
            r
        };
        let ctx = self.finallys.pop().expect("just pushed");
        self.try_depth -= 1;
        r?;
        // Normal completion.
        self.emit(Op::PopHandler);
        self.emit_const_num(0);
        self.emit(Op::StoreLocal(kind_s));
        let j_normal = self.emit(Op::Jump(0));
        // Throw pad: the exception is on the stack.
        let pad = self.ops.len() as u32;
        match &mut self.ops[push] {
            Op::PushHandler(t) => *t = pad,
            _ => unreachable!(),
        }
        self.emit(Op::StoreLocal(val_s));
        self.emit_const_num(1);
        self.emit(Op::StoreLocal(kind_s));
        // The finally block (the pad falls through into it).
        self.patch(j_normal);
        let fin_pc = self.ops.len() as u32;
        for j in &ctx.fin_jumps {
            match &mut self.ops[*j] {
                Op::Jump(t) => *t = fin_pc,
                _ => unreachable!("finally entry is a jump"),
            }
        }
        self.scopes.push(Vec::new());
        let fr = self.block_body(fin);
        self.scopes.pop();
        fr?;
        // Dispatch the pending completion (route 0 falls out the bottom).
        let mut routes: Vec<(u32, Option<Exit>)> = vec![(1, None)];
        if ctx.has_return {
            routes.push((2, Some(Exit::Return)));
        }
        routes.extend(ctx.routes.iter().map(|&(r, e)| (r, Some(e))));
        for (route, exit) in routes {
            self.emit(Op::LoadLocal(kind_s));
            self.emit_const_num(route);
            self.emit(Op::StrictEq);
            let skip = self.emit(Op::JumpIfFalse(0));
            match exit {
                None => {
                    self.emit(Op::LoadLocal(val_s));
                    self.emit(Op::Throw);
                }
                Some(Exit::Return) => {
                    self.emit(Op::LoadLocal(val_s));
                    self.emit_return_tail()?;
                }
                Some(Exit::Break(idx)) => self.emit_jump_exit(idx, false)?,
                Some(Exit::Continue(idx)) => self.emit_jump_exit(idx, true)?,
            }
            self.patch(skip);
        }
        Ok(())
    }
}
