//! Captured block-scoped bindings on the bytecode VM.
//!
//! A `let`/`const`/`class`/catch-parameter binding declared by a block, a loop head or a catch
//! clause that an inner function captures needs a *fresh* binding per block entry (per
//! iteration for loop heads — CreatePerIterationEnvironment), which one function-wide
//! activation cannot express. Such a scope gets a real [`Env`] at run time: a child of the
//! enclosing block env (or of the frame env), holding exactly the scope's captured bindings.
//! The env lives in a hidden slot, wrapped in a *carrier* — an internal, never-exposed function
//! object whose closure environment is the block env (a slot holds `Value`s, and the carrier
//! keeps the env visible to the cycle collector through the ordinary closure edge). Addressing
//! is static: the compiler knows which slot holds each scope's carrier, so exceptions,
//! `break`/`continue` and suspensions need no env bookkeeping at all.
//!
//! Ops (see [`Op`](super::Op)): `BlkNew(dst, parent)` creates the env (parent `u16::MAX` = the
//! frame env) and `BlkDecl(slot, n, const)` declares one binding in TDZ; `BlkCopy(slot)` is
//! CreatePerIterationEnvironment (a sibling env with every binding copied); `BlkLoad`,
//! `BlkStore`, `BlkInit` and `BlkUpdate` access one binding; `InEnv(slot)` runs the closure-,
//! class- or method-creating op that follows it with the block env as the new function's scope.
use super::{step_and_store, Bail, Compiler, Op, PushValue, BLK_BIT};
use crate::interpreter::{new_scope, Abrupt, Binding, Env, Interp};
use crate::value::{Callable, Object, Value};
use std::rc::Rc;

thread_local! {
    /// The function template every carrier shares (never called: carriers never reach JS).
    static CARRIER_FN: Rc<crate::ast::Function> = {
        let body = crate::parser::parse_script("(function(){})", true)
            .expect("carrier template parses");
        match body.into_iter().next() {
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Func(f))) => f,
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Paren(p))) => match *p {
                crate::ast::Expr::Func(f) => f,
                _ => unreachable!("carrier template is a function expression"),
            },
            _ => unreachable!("carrier template is a function expression"),
        }
    };
}

/// Wrap a block env in a carrier value (see the module docs).
fn carrier(env: Env) -> Value {
    let o = Object::new(None);
    o.borrow_mut().call = Callable::user(CARRIER_FN.with(|f| f.clone()), env);
    Value::Obj(o)
}

/// `f` with the block env a carrier slot holds, borrowed (no refcount traffic).
#[inline]
fn with_env<R>(v: &Value, f: impl FnOnce(&Env) -> R) -> R {
    match v {
        Value::Obj(o) => match &o.borrow().call {
            Callable::User(u) => f(&u.env),
            _ => unreachable!("block env slot holds a carrier"),
        },
        _ => unreachable!("block env slot holds a carrier"),
    }
}

/// The block env a carrier slot holds.
#[inline]
pub(super) fn env_of(v: &Value) -> Env {
    match v {
        Value::Obj(o) => match &o.borrow().call {
            Callable::User(u) => u.env.clone(),
            _ => unreachable!("block env slot holds a carrier"),
        },
        _ => unreachable!("block env slot holds a carrier"),
    }
}

/// `BlkNew(dst, parent)`.
pub(super) fn new_env(frame_env: &Env, slots: &mut [Value], dst: u16, parent: u16) {
    let parent = if parent == u16::MAX {
        frame_env.clone()
    } else {
        env_of(&slots[parent as usize])
    };
    set_env(&mut slots[dst as usize], new_scope(Some(parent)));
}

/// Point the carrier in `slot` at `env`: in place when the slot already holds a carrier (from
/// an earlier entry of the same block; it never escapes to JS), else a new one.
fn set_env(slot: &mut Value, env: Env) {
    if let Value::Obj(o) = slot {
        let mut ob = o.borrow_mut();
        if let Callable::User(u) = &mut ob.call {
            let is_carrier = CARRIER_FN.with(|f| Rc::ptr_eq(f, &u.func));
            if let Some(u) = Rc::get_mut(u).filter(|_| is_carrier) {
                let old = std::mem::replace(&mut u.env, env);
                drop(ob);
                drop(old);
                return;
            }
        }
    }
    *slot = carrier(env);
}

/// `BlkDecl(slot, name, is_const)`: an uninitialized (TDZ) binding.
pub(super) fn declare(slots: &[Value], slot: u16, name: &Rc<str>, is_const: bool) {
    let env = env_of(&slots[slot as usize]);
    env.borrow_mut()
        .vars
        .insert(name.clone(), Binding::data(Value::Undefined, !is_const, false));
}

/// `BlkCopy(slot)`: CreatePerIterationEnvironment — a fresh sibling env (same parent) holding a
/// copy of every binding.
pub(super) fn copy(slots: &mut [Value], slot: u16) {
    // Nothing else holds the env (no closure, child env or suspended frame captured it this
    // iteration): a copy would be indistinguishable from it, so it serves the next one too.
    if with_env(&slots[slot as usize], |e| Rc::strong_count(e) == 1) {
        return;
    }
    let fresh = with_env(&slots[slot as usize], |old| {
        let b = old.borrow();
        let e = new_scope(b.parent.clone());
        e.borrow_mut().vars = b.vars.copy_all();
        e
    });
    set_env(&mut slots[slot as usize], fresh);
}

#[cold]
#[inline(never)]
fn tdz(i: &mut Interp, name: &str) -> Abrupt {
    i.throw(
        "ReferenceError",
        format!("cannot access '{name}' before initialization"),
    )
}

/// `BlkLoad(slot, name)`.
#[inline]
pub(super) fn load(i: &mut Interp, slots: &[Value], slot: u16, name: &Rc<str>) -> Result<Value, Abrupt> {
    match load_opt(slots, slot, name) {
        Some(v) => Ok(v),
        None => Err(tdz(i, name)),
    }
}

/// The binding's value, `None` in its TDZ.
#[inline]
pub(super) fn load_opt(slots: &[Value], slot: u16, name: &Rc<str>) -> Option<Value> {
    with_env(&slots[slot as usize], |env| {
        let b = env.borrow();
        let bd = b.vars.get_rc(name).expect("block binding declared");
        bd.initialized.then(|| bd.value.clone())
    })
}

/// `BlkStore(slot, name)` (assignment: TDZ is a ReferenceError, a `const` a TypeError) and
/// `BlkInit(slot, name)` (the declaration's initialization).
pub(super) fn store(
    i: &mut Interp,
    slots: &[Value],
    slot: u16,
    name: &Rc<str>,
    v: Value,
    init: bool,
) -> Result<(), Abrupt> {
    // 0 = stored, 1 = TDZ, 2 = const. The old value drops after the borrows end.
    let (r, old) = with_env(&slots[slot as usize], |env| {
        let mut b = env.borrow_mut();
        let bd = b.vars.get_rc_mut(name).expect("block binding declared");
        if init {
            bd.initialized = true;
            return (0, Some(std::mem::replace(&mut bd.value, v)));
        }
        if !bd.initialized {
            return (1, None);
        }
        if !bd.mutable {
            return (2, None);
        }
        (0, Some(std::mem::replace(&mut bd.value, v)))
    });
    drop(old);
    match r {
        0 => Ok(()),
        1 => Err(tdz(i, name)),
        _ => Err(const_assign(i)),
    }
}

#[cold]
#[inline(never)]
fn const_assign(i: &mut Interp) -> Abrupt {
    i.throw("TypeError", "Assignment to constant variable.")
}

/// `BlkUpdate` on an initialized, mutable Number binding, in place: `Some(result)` (the value
/// the expression produces, `None` for a discarded one), or `None` when the general path must
/// run (TDZ, `const`, a non-Number).
#[inline]
pub(super) fn update_num(
    slots: &[Value],
    slot: u16,
    name: &Rc<str>,
    kind: super::UpdKind,
) -> Option<Option<f64>> {
    use super::UpdKind as K;
    with_env(&slots[slot as usize], |env| {
        let mut b = env.borrow_mut();
        let bd = b.vars.get_rc_mut(name)?;
        let Value::Num(x) = bd.value else { return None };
        if !bd.initialized || !bd.mutable {
            return None;
        }
        let inc = matches!(kind, K::PreInc | K::PostInc | K::IncDiscard);
        let y = if inc { x + 1.0 } else { x - 1.0 };
        bd.value = Value::Num(y);
        Some(match kind {
            K::PreInc | K::PreDec => Some(y),
            K::PostInc | K::PostDec => Some(x),
            K::IncDiscard | K::DecDiscard => None,
        })
    })
}

/// `BlkUpdate(slot, name, kind)`: `++`/`--` on a (mutable) block binding.
pub(super) fn update(
    i: &mut Interp,
    stack: &mut impl PushValue,
    slots: &[Value],
    slot: u16,
    name: &Rc<str>,
    kind: super::UpdKind,
) -> Result<(), Abrupt> {
    let old = load(i, slots, slot, name)?;
    let carrier = &slots[slot as usize];
    step_and_store(i, stack, kind, old, |i, v| {
        let (ok, old) = with_env(carrier, |env| {
            let mut b = env.borrow_mut();
            let bd = b.vars.get_rc_mut(name).expect("block binding declared");
            if !bd.mutable {
                return (false, None);
            }
            (true, Some(std::mem::replace(&mut bd.value, v)))
        });
        drop(old);
        if !ok {
            return Err(const_assign(i));
        }
        Ok(())
    })
}

impl Compiler {
    /// Open a block env holding `names` ((name, is_const), declared in TDZ) as a child of the
    /// innermost enclosing one, bind them in the current (already pushed) scope, and make it
    /// the innermost. Returns the carrier slot and the parent operand (for re-creation per
    /// iteration, see [`Compiler::blk_renew`]). The caller truncates `blk_envs` on scope exit.
    pub(super) fn blk_open(&mut self, names: &[(String, bool)]) -> Result<(u16, u16), Bail> {
        if self.slot_names.len() >= (BLK_BIT - 1) as usize {
            return Err(Bail);
        }
        let slot = self.fresh_slot("%blk%");
        let parent = self.blk_envs.last().copied().unwrap_or(u16::MAX);
        self.emit(Op::BlkNew(slot, parent));
        for (n, k) in names {
            let ni = self.name_idx(n);
            self.emit(Op::BlkDecl(slot, ni, *k));
            self.scope_bind(n, slot | BLK_BIT, *k);
        }
        self.blk_envs.push(slot);
        Ok((slot, parent))
    }

    /// A fresh env (same parent, same declarations in TDZ) in carrier slot `slot` — a loop
    /// head's binding for the next iteration.
    pub(super) fn blk_renew(&mut self, slot: u16, parent: u16, names: &[(String, bool)]) {
        self.emit(Op::BlkNew(slot, parent));
        for (n, k) in names {
            let ni = self.name_idx(n);
            self.emit(Op::BlkDecl(slot, ni, *k));
        }
    }

    /// Open a block env for the captured declarations queued by `declare_lexical_pattern` /
    /// `declare_block_lexicals` (`pending_blk`), if any.
    #[allow(clippy::type_complexity)]
    pub(super) fn blk_flush(&mut self) -> Result<Option<(u16, u16, Vec<(String, bool)>)>, Bail> {
        if self.pending_blk.is_empty() {
            return Ok(None);
        }
        let names = std::mem::take(&mut self.pending_blk);
        let (slot, parent) = self.blk_open(&names)?;
        Ok(Some((slot, parent, names)))
    }

    /// Before an op that creates a function or class: route its scope through the innermost
    /// block env.
    pub(super) fn env_prefix(&mut self) {
        if let Some(&s) = self.blk_envs.last() {
            self.emit(Op::InEnv(s));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::{Completion, Engine};

    /// Evaluate `src` on the tree-walker and on the bytecode tier (threshold 0), requiring the
    /// same result; returns it.
    fn both(src: &str) -> String {
        let mut out = Vec::new();
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut e = Engine::new();
            e.interp.tier = tier;
            e.interp.tier_threshold = 0;
            out.push(match e.eval(src, false).expect("parse") {
                Completion::Value(v) => v,
                Completion::Throw { name, message } => format!("threw {name}: {message}"),
            });
        }
        assert_eq!(out[0], out[1], "tiers disagree on {src}");
        out.pop().expect("two results")
    }

    #[test]
    fn captured_loop_and_block_bindings_are_fresh_per_entry() {
        let r = both(
            "'use strict';
            function t1() { const fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); return fs.map(f => f()).join(); }
            function t2() { const fs = []; for (const x of [1, 2]) fs.push(() => x); return fs.map(f => f()).join(); }
            function t3() { const fs = []; for (const k in { a: 1, b: 2 }) fs.push(() => k); return fs.map(f => f()).join(); }
            function t4() { const fs = []; for (const [a, b] of [[1, 2], [3, 4]]) fs.push(() => a + b); return fs.map(f => f()).join(); }
            function t5() { const fs = []; for (let i = 0; i < 2; i++) { let j = i * 2; fs.push(() => j); } return fs.map(f => f()).join(); }
            function t6() { try { throw 5; } catch (e) { return (() => e)(); } }
            function t7() { const fs = []; for (let i = 0; i < 2; i++) { class C { m() { return i; } } fs.push(new C()); } return fs.map(c => c.m()).join(); }
            function t8() { try { let f = () => y; f(); let y = 1; } catch (e) { return e.constructor.name; } }
            function t9() { try { for (const x of [1]) { const g = () => x; x = 2; } } catch (e) { return e.constructor.name; } }
            function t10() { const fs = []; for (let { a, b } = { a: 0, b: 2 }; a < b; a++) fs.push(() => a); return fs.map(f => f()).join(); }
            function t11(k) { { function inner(x) { return x <= 0 ? 'z' : inner(x - 1) + k; } return inner(2); } }
            [t1(), t2(), t3(), t4(), t5(), t6(), t7(), t8(), t9(), t10(), t11('k')].join('|')",
        );
        assert_eq!(r, "0,1,2|1,2|a,b|3,7|0,2|5|0,1|ReferenceError|TypeError|0,1|zkk");
    }

    #[test]
    fn tail_calls_labels_destructuring_and_spread_new() {
        let r = both(
            "'use strict';
            const tco = function f(n) { return n === 0 ? 'done' : f(n - 1); };
            const tco2 = function g(n, acc) { if (n === 0) return acc; return g(n - 1, acc + 1); };
            function lbl() { let s = 0; blk: { s++; if (s) break blk; s = 100; } return s; }
            function nest() { const log = []; const it = (n) => ({ [Symbol.iterator]() { return this; }, i: 0,
                next() { return { value: this.i++, done: false }; }, return() { log.push(n); return {}; } });
                for (const a of it('A')) for (const b of it('B')) return log.join() + ';' + a + b; }
            function nest2() { const log = []; const it = (n) => ({ [Symbol.iterator]() { return this; }, i: 0,
                next() { return { value: this.i++, done: false }; }, return() { log.push(n); return {}; } });
                out: for (const a of it('A')) for (const b of it('B')) break out; return log.join(); }
            function ds() { const [{ a }, [b, c = 9], ...r] = [{ a: 1 }, [2], 3, 4]; return [a, b, c, r].join(); }
            class P { constructor(...a) { this.a = a; } }
            function ns(xs) { return new P(1, ...xs).a.join(); }
            function rest(...r) { return () => r.length; }
            [tco(100000), tco2(100000, 0), lbl(), nest(), nest2(), ds(), ns([2, 3]), rest(1, 2)()].join('|')",
        );
        assert_eq!(r, "done|100000|1|;00|B,A|1,2,9,3,4|1,2,3|2");
    }
}

