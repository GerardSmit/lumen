//! Derived-class constructors (`class B extends A { constructor() { …; super(…); … } }`).
//!
//! A body that contains a `super(…)` call compiles in *derived mode* (`Chunk::derived`): its
//! `this` lives in the activation environment exactly like the tree-walker's — a binding that
//! starts in TDZ and is initialized by `super(…)` (possibly from a nested arrow, which reaches the
//! same binding through its scope chain), so every `this` read is a lexical, TDZ-checked read.
//! `super(…)` itself is two ops around the argument evaluation, sharing the oracle's
//! `super_call_prepare` / `super_call_complete` halves (legality checks and GetSuperConstructor
//! before the arguments; IsConstructor, the parent construct with the active new.target,
//! BindThisValue with the base-return override, and the field initializers after). Every
//! completion goes through `Op::DerivedReturn`, the [[Construct]] return rule.
//!
//! The runtime only runs a derived-mode chunk for a derived class construct (and never runs a
//! non-derived chunk for one); see `Interp::call_user_inner`.
use super::{ArrayElem, CResult, Compiler, Op};
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

impl Compiler {
    /// `super(args)`: only in a derived-mode body (an arrow's `super()` stays in the oracle).
    pub(super) fn super_call(&mut self, args: &[ArrayElem]) -> CResult {
        if !self.derived {
            return Err(super::Bail);
        }
        self.emit(Op::SuperCtor);
        let argc = args.len() as u16;
        if self.call_args(args)? {
            self.emit(Op::SuperCallSpread(argc));
        } else {
            self.emit(Op::SuperCall(argc));
        }
        Ok(())
    }
}

/// `Op::SuperCtor`: push the parent constructor, then the `this` value it will run on.
pub(super) fn super_ctor(i: &mut Interp, env: &Env, stack: &mut Vec<Value>) -> Result<(), Abrupt> {
    let (parent, this) = i.super_call_prepare(env)?;
    stack.push(parent);
    stack.push(this);
    Ok(())
}

/// `Op::SuperCall` / `Op::SuperCallSpread`: pops `argc` arguments (the last one an iterable to
/// expand when `spread`), the `this` and the parent pushed by `SuperCtor`; pushes the bound
/// `this`.
pub(super) fn super_call(
    i: &mut Interp,
    env: &Env,
    stack: &mut Vec<Value>,
    argc: usize,
    spread: bool,
) -> Result<(), Abrupt> {
    let mut argv = stack.split_off(stack.len() - argc);
    if spread {
        let last = argv.pop().expect("spread super() has an argument");
        let items = if i.in_default_derived_ctor(env) {
            i.default_ctor_args(&last)?
        } else if let Some(items) = super::iter_fast::values(i, &last) {
            items
        } else {
            i.iterate(&last)?
        };
        argv.extend(items);
    }
    let this = stack.pop().expect("super() this");
    let parent = stack.pop().expect("super() parent");
    let v = i.super_call_complete(env, parent, this, argv)?;
    stack.push(v);
    Ok(())
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

    fn compiled(src: &str, name: &str) -> bool {
        let mut e = Engine::new();
        e.set_tier_threshold(0);
        let _ = e.eval(src, false).unwrap();
        let v = e.interp.global.borrow().props.get(name).unwrap().value();
        let o = v.as_obj().unwrap().borrow();
        let Callable::User(u) = &o.call else {
            panic!("user function")
        };
        matches!(u.func.code.get(), Some(Some(c)) if c.derived)
    }

    #[test]
    fn derived_constructor_semantics_match_the_oracle() {
        let cases = [
            // plain, fields after super, super value, arrows reading this
            "class A { constructor(x){ this.a = x; } } class B extends A { f = this.a + 1; \
             constructor(x){ const r = super(x * 2); this.same = r === this; \
             const g = () => this.a; this.g = g(); } } \
             const b = new B(3); [b.a, b.f, b.same, b.g, b instanceof B].join()",
            // this before super → ReferenceError
            "class A {} class B extends A { constructor(){ this.x = 1; super(); } } \
             try { new B(); 'no' } catch (e) { e.constructor.name }",
            // super twice → ReferenceError after the parent ran
            "var n = 0; class A { constructor(){ n++; } } \
             class B extends A { constructor(){ super(); super(); } } \
             try { new B(); 'no' } catch (e) { e.constructor.name + n }",
            // return override rules
            "class A {} class B extends A { constructor(){ super(); return {k: 1}; } } new B().k",
            "class A {} class B extends A { constructor(){ super(); return 1; } } \
             try { new B(); 'no' } catch (e) { e.constructor.name }",
            "class A {} class B extends A { constructor(){ return undefined; } } \
             try { new B(); 'no' } catch (e) { e.constructor.name }",
            "class A {} class B extends A { constructor(){ return {z: 2}; } } new B().z",
            // the TypeError is not catchable inside the body
            "class A {} class B extends A { constructor(){ super(); try { return 1; } catch (e) \
             { return {caught: 1}; } } } try { new B(); 'no' } catch (e) { e.constructor.name }",
            // base constructor returning an object replaces this
            "var o = {tag: 'o'}; class A { constructor(){ return o; } } \
             class B extends A { y = 5; constructor(){ super(); this.z = this === o; } } \
             const b = new B(); [b === o, b.y, b.z].join()",
            // new.target propagates to the parent
            "var nt; class A { constructor(){ nt = new.target; } } class B extends A {} \
             class C extends B { constructor(...a){ super(...a); } } new C(); nt === C",
            // spread arguments, default ctor, finally
            "class A { constructor(...a){ this.s = a.join('-'); } } \
             class B extends A { constructor(){ try { super(...[1, 2], 3); } finally { this.f = 1; } } } \
             const b = new B(); b.s + b.f",
            // super from an arrow initializes the same binding
            "class A { constructor(){ this.p = 1; } } class B extends A { constructor(){ \
             const s = () => super(); s(); this.q = this.p + 1; } } new B().q",
            // super property access after super()
            "class A { m(){ return 'am'; } get g(){ return 'ag'; } } class B extends A { \
             constructor(){ super(); this.r = super.m() + super.g; } } new B().r",
            // non-constructor parent
            "function P(){} class B extends P { constructor(){ super(); } } \
             Object.setPrototypeOf(B, Math.max); try { new B(); 'no' } catch (e) { e.constructor.name }",
            // native parent
            "class E extends Error { constructor(m){ super(m); this.name = 'E'; } } \
             const e = new E('boom'); [e.message, e instanceof Error, e.name].join()",
            "class M extends Map { constructor(){ super([[1, 2]]); } } new M().get(1)",
        ];
        for src in cases {
            run(src);
        }
    }

    #[test]
    fn derived_constructors_compile() {
        assert!(compiled(
            "class A {} class B extends A { constructor(x){ super(); this.x = +x; } } \
             globalThis.B = B; new B(1);",
            "B"
        ));
    }
}
