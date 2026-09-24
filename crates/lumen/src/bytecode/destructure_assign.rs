//! Destructuring assignment (`[a, b] = [b, a]`, `({x, y: o} = src)`).
//!
//! The target converts to the same [`Pattern`] a declaration uses (the parser's cover-grammar
//! refinement), and lowers through `destructure_store` in *assignment mode*
//! (`Compiler::assign_mode`): every leaf is a PutValue on an identifier reference — a slot, an
//! activation binding or a free name, exactly the plain `name = v` lowering — instead of a
//! binding initialization. Member-expression leaves (`[o.x] = …`) bail: their Reference must
//! be evaluated before the element's value is read, which the store-after-read shape can't
//! express.
use super::{Bail, CResult, Compiler, Home, Op};
use crate::ast::Expr;

impl Compiler {
    /// `target = value` with an array/object literal target; `keep`: leave the RHS value (the
    /// expression's result) on the stack.
    pub(super) fn destructure_assign(&mut self, target: &Expr, value: &Expr, keep: bool) -> CResult {
        let pat = crate::parser::destructuring_target(target).ok_or(Bail)?;
        self.expr(value)?;
        if keep {
            self.emit(Op::Dup);
        }
        let saved = std::mem::replace(&mut self.assign_mode, true);
        let r = self.destructure_store(&pat, crate::ast::DeclKind::Var);
        self.assign_mode = saved;
        r
    }

    /// Assignment-mode leaf: PutValue(name, <top of stack>), consuming it.
    pub(super) fn assign_leaf(&mut self, name: &str) -> CResult {
        match self.home(name) {
            Some(Home::Slot(slot, is_const)) => {
                if is_const || self.tdz_pending.contains(&slot) {
                    return Err(Bail);
                }
                self.emit(Op::StoreLocal(slot));
            }
            Some(Home::Env(is_const)) => {
                if is_const {
                    return Err(Bail);
                }
                let n = self.name_idx(name);
                self.emit(Op::StoreCap(n));
            }
            Some(Home::Blk(c, _)) => {
                let n = self.name_idx(name);
                self.emit(Op::BlkStore(c, n));
            }
            None => {
                let n = self.name_idx(name);
                self.emit_store_name(n);
            }
        }
        Ok(())
    }
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
    fn destructuring_assignment_matches_the_oracle() {
        let cases = [
            "function f(){ let a = 1, b = 2; [a, b] = [b, a]; return [a, b].join(); } f()",
            "function f(){ let a, b; const r = ([a, , b] = 'xyz'); return a + b + typeof r; } f()",
            "function f(o){ let x, y, z; ({x, y: z = 5, ...y} = o); return JSON.stringify([x, y, z]); } \
             f({x: 1, q: 2})",
            "var g1, g2; function f(){ ({a: g1, b: {c: g2}} = {a: 1, b: {c: 2}}); return g1 + g2; } f()",
            "function f(){ let c = 0; const k = () => c; ({c} = {c: 3}); return k(); } f()",
            "function f(){ let a; try { ({a} = null); } catch (e) { return e.constructor.name; } } f()",
            "function f(){ const a = 1; try { [a] = [2]; } catch (e) { return e.constructor.name; } } f()",
            "function f(){ let a, b; [a, a] = [1, 2]; return a; } f()",
            "function f(){ let fn; ({fn = function(){}} = {}); return fn.name; } f()",
            "function f(){ let a = 0, b = 0; for (let i = 0; i < 5; i++) [a, b] = [b, a + i]; \
             return a + ',' + b; } f()",
            "function f(){ let a, b; [a, b] = new Set([7, 8]); return a * b; } f()",
            "function f(){ 'use strict'; try { ({u: undeclaredX} = {u: 1}); } \
             catch (e) { return e.constructor.name; } } f()",
            "function f(o){ let a; [o.p, a] = [1, 2]; return o.p + a; } f({})",
            "function f(){ var a, b; var r = [a, b] = [3, 4]; return r.length + a + b; } f()",
            "function f(){ var [a, a] = [1, 2]; return a; } f()",
        ];
        for src in cases {
            run(src);
        }
    }

    #[test]
    fn a_swap_loop_compiles() {
        let mut e = Engine::new();
        e.set_tier_threshold(0);
        let _ = e
            .eval(
                "function swap(n){ let a = 1, b = 2; for (let i = 0; i < n; i++) [a, b] = [b, a]; \
                 return a; } swap(3)",
                false,
            )
            .unwrap();
        let v = e.interp.global.borrow().props.get("swap").unwrap().value();
        let o = v.as_obj().unwrap().borrow();
        let Callable::User(u) = &o.call else {
            panic!("user function")
        };
        assert!(matches!(u.func.code.get(), Some(Some(_))));
    }
}
