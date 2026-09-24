//! Sync generator bodies (`function*`) on the bytecode VM.
//!
//! A compiled generator runs on a [`VmCoro`](super::VmCoro) instead of an OS-thread coroutine:
//! suspension is `run_vm` returning with the frame state (slots, operand stack, pc, handlers)
//! saved in the coroutine, exactly like an async body's `await`. The ops:
//!
//! * `InitialYield` ends the call-time prologue (parameter binding, hoisting): the call runs the
//!   body up to it synchronously — so a parameter initializer's throw propagates from the call —
//!   and parks there (suspendedStart) before handing out the generator object.
//! * `Yield` pops the yielded value and parks. The resume pushes the received value and a
//!   *returning* flag: `next(v)` → `[v, false]`; `return(v)` → `[v, true]`, and the flag's
//!   branch runs the ordinary `return` lowering (`finally` blocks, for-of closes) from the yield
//!   point; `throw(e)` is injected as a throw at the yield (handlers unwind as usual).
//! * `YieldDelegate(it)` is `yield*`'s whole loop over the inner iterator held in slots `it`
//!   (object) and `it + 1` (its `next`). It consumes `[received, mode]` (0 next / 1 throw /
//!   2 return), forwards to the inner iterator per 14.4.14, and either finishes — pushing the
//!   inner result's value and the returning flag, like `Yield` — or parks *on itself* (the pc
//!   is rewound) forwarding the inner result object unwrapped; its resume pushes
//!   `[received, mode]` and the op runs again.
//!
//! Suspension reuses [`VmStep::Await`](super::VmStep): `VmCoro` tells a yield from an await
//! by the op it parked at.
use super::{CResult, Compiler, Op};
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;

impl Compiler {
    /// `yield arg` / `yield* arg` (`yield*` in sync generators only).
    pub(super) fn yield_expr(&mut self, delegate: bool, arg: Option<&crate::ast::Expr>) -> CResult {
        if !self.generator || (delegate && self.async_gen) {
            return Err(super::Bail);
        }
        match arg {
            Some(a) => self.expr(a)?,
            None => {
                self.emit(Op::Undef);
            }
        }
        if delegate {
            let it = self.fresh_slot("%deleg_iter%");
            let next = self.fresh_slot("%deleg_next%");
            debug_assert_eq!(next, it + 1);
            self.emit(Op::GetIter);
            self.emit(Op::StoreLocal(next));
            self.emit(Op::StoreLocal(it));
            self.emit(Op::Undef);
            let k = self.const_idx(Value::Num(0.0));
            self.emit(Op::Const(k));
            self.emit(Op::YieldDelegate(it));
        } else {
            // AsyncGeneratorYield awaits the operand first (a rejection throws at the yield).
            if self.async_gen {
                self.emit(Op::Await);
            }
            self.emit(Op::Yield);
        }
        // [value, returning]: a `return(v)` resumption completes like `return v` from here —
        // in an async generator after awaiting `v` (AsyncGeneratorUnwrapYieldResumption: a
        // rejection is a throw at the yield).
        let normal = self.emit(Op::JumpIfFalse(0));
        if self.async_gen {
            self.emit(Op::Await);
        }
        self.emit_return_tail()?;
        self.patch(normal);
        Ok(())
    }
}

/// One `Op::YieldDelegate` step over the inner iterator in `slots[it]` / `slots[it + 1]`, with
/// `[received, mode]` on top of `stack`. `Ok(None)`: the delegation finished and `[value,
/// returning]` was pushed. `Ok(Some(result))`: park, forwarding the inner result object.
pub(super) fn delegate_step(
    i: &mut Interp,
    slots: &[Value],
    it: u16,
    stack: &mut Vec<Value>,
) -> Result<Option<Value>, Abrupt> {
    let mode = match stack.pop() {
        Some(Value::Num(n)) => n as u8,
        _ => 0,
    };
    let received = stack.pop().unwrap_or(Value::Undefined);
    let iterator = slots[it as usize].clone();
    let (result, returning) = match mode {
        0 => {
            let next = slots[it as usize + 1].clone();
            (i.call(next, iterator.clone(), &[received])?, false)
        }
        1 => {
            // GetMethod(iterator, "throw"): absent → close the inner iterator (close errors
            // propagate), then a protocol TypeError.
            let throw = i.get_member(&iterator, "throw")?;
            if matches!(throw, Value::Undefined | Value::Null) {
                i.iterator_close_normal(&iterator)?;
                return Err(i.throw("TypeError", "the delegated iterator has no 'throw' method"));
            }
            if !throw.is_callable() {
                return Err(i.throw("TypeError", "iterator 'throw' is not callable"));
            }
            (i.call(throw, iterator.clone(), &[received])?, false)
        }
        _ => {
            let ret = i.get_member(&iterator, "return")?;
            if matches!(ret, Value::Undefined | Value::Null) {
                stack.push(received);
                stack.push(Value::Bool(true));
                return Ok(None);
            }
            if !ret.is_callable() {
                return Err(i.throw("TypeError", "iterator 'return' is not callable"));
            }
            (i.call(ret, iterator.clone(), &[received])?, true)
        }
    };
    if !matches!(result, Value::Obj(_)) {
        return Err(i.throw("TypeError", "iterator result is not an object"));
    }
    let done = i.get_member(&result, "done")?;
    if i.to_boolean(&done) {
        let v = i.get_member(&result, "value")?;
        stack.push(v);
        stack.push(Value::Bool(returning));
        return Ok(None);
    }
    // GeneratorYield forwards the inner result object as-is (the driver must not re-wrap it).
    i.yield_raw_result = true;
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::value::Callable;
    use crate::{Completion, Engine};

    fn run(src: &str) -> String {
        let mut out = Vec::new();
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut e = Engine::new();
            e.set_tier(tier);
            e.set_tier_threshold(0);
            out.push(match e.eval(src, false).unwrap() {
                Completion::Value(v) => v.to_string(),
                Completion::Throw { name, message } => format!("throw {name}: {message}"),
            });
        }
        assert_eq!(out[0], out[1], "tiers disagree on {src}");
        out.pop().unwrap()
    }

    #[test]
    fn generator_semantics_match_the_oracle() {
        let cases = [
            "function* g(){ const a = yield 1; const b = yield a + 1; return a + b; } \
             const it = g(); JSON.stringify([it.next('x'), it.next(10), it.next(5), it.next()])",
            "function* g(){ try { yield 1; yield 2; } finally { log.push('fin'); } } var log = []; \
             const it = g(); it.next(); JSON.stringify([it.return(7), it.next(), log])",
            "function* g(){ try { yield 1; } catch (e) { yield 'c' + e; } return 'end'; } \
             const it = g(); JSON.stringify([it.next(), it.throw('E'), it.next()])",
            "function* g(){ yield 1; } const it = g(); \
             try { it.throw(new Error('pre')); } catch (e) { log = e.message } \
             var log; JSON.stringify([log, it.next()])",
            "function* g(){ yield 1; } const it = g(); JSON.stringify([it.return(3), it.next()])",
            "function* inner(){ const x = yield 'i1'; yield x; return 'ret'; } \
             function* g(){ const r = yield* inner(); yield r; } \
             const it = g(); JSON.stringify([it.next(), it.next('X'), it.next(), it.next()])",
            "function* g(){ yield* [1, 2, 3]; } [...g()].join()",
            "function* inner(){ try { yield 1; yield 2; } finally { log.push('inner-fin'); } } \
             function* g(){ try { yield* inner(); } finally { log.push('outer-fin'); } } var log = []; \
             const it = g(); it.next(); JSON.stringify([it.return(9), log])",
            "function* inner(){ try { yield 1; } catch (e) { yield 'caught ' + e; } } \
             function* g(){ yield* inner(); } const it = g(); it.next(); \
             JSON.stringify([it.throw('T'), it.next()])",
            "function* g(){ for (const x of [1, 2, 3]) { yield x * 2; } } \
             const it = g(); it.next(); JSON.stringify([it.return(0), it.next()])",
            "function* g(a = (() => { throw new Error('param'); })()){ yield a; } \
             try { g(); 'no' } catch (e) { e.message }",
            "function* g(){ let i = 0; while (true) { const cmd = yield i++; if (cmd === 'stop') return 'stopped'; } } \
             const it = g(); it.next(); it.next(); JSON.stringify([it.next('stop'), it.next()])",
            "function* g(){ yield this.v; } JSON.stringify(g.call({v: 42}).next())",
            "function* g(){ try { return 1; } finally { yield 2; } } const it = g(); \
             JSON.stringify([it.next(), it.next(), it.next()])",
            "function* g(){ try { yield 1; } finally { return 'override'; } } const it = g(); it.next(); \
             JSON.stringify(it.return('r'))",
            "function* g(){ const it = g2(); function* g2(){ yield 1; } yield* it; yield arguments.length; } \
             [...g(1, 2)].join()",
            "function* g(){ yield 1; } const it = g(); it.next(); \
             let e; try { it.next.call({}); } catch (x) { e = x.constructor.name } e",
            "function* g(){ yield* { [Symbol.iterator](){ return { next(){ return 1; } }; } }; } \
             try { g().next(); 'no' } catch (e) { e.constructor.name }",
            "function* g(){ yield* { [Symbol.iterator](){ return { next(){ return {done: false, value: 1}; } }; } }; } \
             const it = g(); it.next(); try { it.throw('x'); 'no' } catch (e) { e.constructor.name }",
            "var g = function*(){ var self = yield; yield self === it; }; var it = g(); it.next(); \
             JSON.stringify(it.next(it))",
        ];
        for src in cases {
            run(src);
        }
    }

    #[test]
    fn generators_compile() {
        let mut e = Engine::new();
        e.set_tier_threshold(0);
        let _ = e
            .eval("function* g(){ yield 1; yield* [2]; } [...g()]", false)
            .unwrap();
        let v = e.interp.global.borrow().props.get("g").unwrap().value();
        let o = v.as_obj().unwrap().borrow();
        let Callable::User(u) = &o.call else {
            panic!("user function")
        };
        assert!(matches!(u.func.code.get(), Some(Some(_))));
    }
}
