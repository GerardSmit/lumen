//! Proper tail calls (ES2015+ §15.10, IsInTailPosition / PrepareForTailCall).
//!
//! In a strict, non-async, non-generator function (`Compiler::tail_calls`), every call in a
//! tail position of a `return` (arrow concise bodies are `return <expr>`) compiles to
//! `Op::TailCall` / `Op::TailCallSpread` followed by `Return`: through `(…)`, the last operand
//! of `,`, both arms of `?:`, the right operand of `&&` / `||` / `??`, and an optional chain
//! ending in a call. `new`, `super(…)`, direct `eval`, tagged templates (compiled by the
//! tree-walker, which runs them as tail calls itself) and every other expression keep ordinary
//! calls. A `return` is a tail position unless code of this frame runs after it: an enclosing
//! `try` block or a `catch`/`try` block with a `finally` (the `finally` block itself is one),
//! a for-of loop (IteratorClose), a derived constructor (the `this` check) — see
//! [`Compiler::tail_position`].
//!
//! At run time (`Op::TailCall` in `run_vm_frames`) the callee replaces the current frame when
//! the frame may tail-call (`Interp::tco_ok`: strict and not `[[Construct]]`): an inline frame
//! is left and the call is made from its caller's call site (inline again when the callee
//! qualifies, so the frame record is reused); the root frame of a `drive_vm` makes the call as
//! an inline frame (constant space from there on); a callee that cannot run inline is called
//! ordinarily while shallow and handed to the `Interp::call` trampoline (`Interp::pending_tail`)
//! past `TAIL_NEST`. Native code's generic call-out of the op (`ONE`) does the same.
//!
//! The optimizing tier (`jit`) does not lower these ops yet: a function containing one stays on
//! the interpreter.
use super::{CResult, Compiler, Op};
use crate::ast::Expr;

impl Compiler {
    /// Whether a `return` here may end in a proper tail call: nothing runs after it in this
    /// frame (no handler, finally, for-of close, derived-constructor check, generator/async
    /// completion).
    pub(super) fn tail_position(&self) -> bool {
        self.tail_calls
            && self.strict
            && !self.generator
            && !self.derived
            && self.try_depth == 0
            && self.finallys.is_empty()
            && self.loops.iter().all(|c| c.foreach_iter.is_none())
    }

    /// `return <e>` in tail position (see [`Compiler::tail_position`]).
    pub(super) fn tail_return(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Paren(inner) => self.tail_return(inner),
            Expr::Cond { test, cons, alt } => {
                let jf = self.jump_if_false(test)?;
                self.tail_return(cons)?;
                self.patch(jf);
                self.tail_return(alt)
            }
            Expr::Seq(items) if !items.is_empty() => {
                let (last, init) = items.split_last().expect("non-empty");
                for ex in init {
                    self.expr_stmt(ex)?;
                }
                self.tail_return(last)
            }
            Expr::Logical { op, left, right } => {
                self.expr(left)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalsePeek(0)),
                    "||" => self.emit(Op::JumpIfTruePeek(0)),
                    "??" => self.emit(Op::JumpIfNotNullishPeek(0)),
                    _ => return Err(super::Bail),
                };
                self.emit(Op::Pop);
                self.tail_return(right)?;
                self.patch(j);
                self.emit_return_tail()
            }
            Expr::Call { .. } => {
                self.expr(e)?;
                self.retag_tail_call();
                self.emit_return_tail()
            }
            // `a?.b(…)` / `f?.(…)`: the chain's final call is in tail position; a short-circuit
            // returns `undefined`.
            Expr::OptionalChain(inner) if matches!(**inner, Expr::Call { .. }) => {
                let mut shorts = Vec::new();
                self.opt_chain(inner, &mut shorts)?;
                self.retag_tail_call();
                self.emit_return_tail()?;
                if !shorts.is_empty() {
                    for j in shorts {
                        self.patch(j);
                    }
                    self.emit(Op::Undef);
                    self.emit_return_tail()?;
                }
                Ok(())
            }
            _ => {
                self.expr(e)?;
                self.emit_return_tail()
            }
        }
    }

    /// Turn the call op just emitted (a call expression's last op) into its tail-call form.
    fn retag_tail_call(&mut self) {
        let last = self.ops.last_mut().expect("a call emits ops");
        *last = match *last {
            Op::Call(n) => Op::TailCall(n, false),
            Op::CallWithThis(n) => Op::TailCall(n, true),
            Op::CallSpread(n) => Op::TailCallSpread(n, false),
            Op::CallSpreadThis(n) => Op::TailCallSpread(n, true),
            other => other,
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::{Completion, Engine};

    fn run(src: &str, tier: Tier) -> String {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => format!("threw {name}: {message}"),
        }
    }

    /// Unbounded tail recursion of every shape runs in constant space on the bytecode tier.
    #[test]
    fn proper_tail_calls_in_every_tail_position() {
        let src = "'use strict';
            const N = 100000;
            function decl(n, acc) { return n === 0 ? acc : decl(n - 1, acc + 1); }
            function even(n) { return n === 0 || odd(n - 1); }
            function odd(n) { return n !== 0 && even(n - 1); }
            const o = { m(n) { return n === 0 ? 'm' : this.m(n - 1); } };
            const arrow = (n) => n === 0 ? 'a' : arrow(n - 1);
            function seq(n) { return (0, n === 0 ? 's' : seq(n - 1)); }
            function nc(n) { return n === 0 ? 'nc' : null ?? nc(n - 1); }
            function opt(n) { return n === 0 ? 'opt' : o2?.f(n - 1); }
            const o2 = { f: opt };
            function sw(n) { switch (n) { case 0: return 'sw'; default: return sw(n - 1); } }
            function cat(n) { try { throw n; } catch (e) { return e === 0 ? 'cat' : cat(e - 1); } }
            function fin(n) { try { } finally { return n === 0 ? 'fin' : fin(n - 1); } }
            function few(a, b) { return a === 0 ? String(b) : few(a - 1); }
            function many(n) { return n === 0 ? arguments.length : many(n - 1, 1, 2); }
            function spread(n) { return n === 0 ? 'sp' : spread(...[n - 1]); }
            class C { static s(n) { return n === 0 ? 'cls' : C.s(n - 1); } }
            function nt() { return new.target === undefined; }
            function Ctor() { return nt(); }
            [decl(N, 0), even(N), odd(N), o.m(N), arrow(N), seq(N), nc(N), opt(N), sw(N), cat(N),
             fin(N), few(N, 1), many(N), spread(N), C.s(N), typeof new Ctor(), Ctor()].join()";
        let want = "100000,true,false,m,a,s,nc,opt,sw,cat,fin,undefined,3,sp,cls,object,true";
        assert_eq!(run(src, Tier::Bytecode), want);
    }

    /// Positions the spec excludes (a `try` block, for-of) and sloppy code keep ordinary calls
    /// (bytecode tier only: the tree-walker would need the deep native stack of a runner thread).
    /// The recursion runs to the engine's depth ceiling (`MAX_EVAL_DEPTH`), which assumes the
    /// large thread stack the CLI and runners give it: with eager JIT frames the default 2 MB
    /// test-thread stack overflows first, so this runs on its own thread like the CLI's main.
    #[test]
    fn non_tail_positions_keep_their_frames() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(non_tail_positions_body)
            .expect("spawn test thread")
            .join()
            .expect("test thread");
    }

    fn non_tail_positions_body() {
        let src = "
            function sloppy(n) { return n === 0 ? 0 : sloppy(n - 1); }
            function inTry(n) { 'use strict'; try { return n === 0 ? 0 : inTry(n - 1); } catch (e) { throw e; } }
            function inForOf(n) { 'use strict'; for (const x of [n]) return x === 0 ? 0 : inForOf(x - 1); }
            function r(f) { try { return String(f(100000)); } catch (e) { return e.name; } }
            [r(sloppy), r(inTry), r(inForOf), inTry(10), inForOf(10)].join()";
        assert_eq!(run(src, Tier::Bytecode), "RangeError,RangeError,RangeError,0,0");
    }

    /// A non-callable callee's TypeError names it as the tree-walker does, in every call shape.
    #[test]
    fn not_a_function_names_the_callee_like_the_tree_walker() {
        let src = "
            const x = 5, o = { p: 1 }, arr = [1];
            function g(v) { return v; }
            const fs = [
                () => { x(); }, function () { 'use strict'; return x(); }, () => { o.p(); },
                () => { arr[0](); }, () => { (x)(); }, () => { (o.p)(); }, () => { g(x)(); },
                () => { (0, x)(); }, () => { o.p?.(); }, () => { x?.(); }, () => { x(...arr); },
                () => { o.p(...arr); }, function () { 'use strict'; return o.p(...arr); },
            ];
            fs.map(f => { try { f(); return 'ok'; } catch (e) { return e.message; } }).join('|')";
        let (x, p, e) = ("x", "(intermediate value).p", "expression");
        let want = [x, x, p, e, x, p, e, e, p, x, x, p, p]
            .map(|d| format!("{d} is not a function"))
            .join("|");
        assert_eq!(run(src, Tier::Interp), want);
        assert_eq!(run(src, Tier::Bytecode), want);
    }
}
