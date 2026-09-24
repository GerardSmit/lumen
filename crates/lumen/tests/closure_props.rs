//! Closures share their function's `length`/`name` entry block until one of them is written.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x, m) {{ if (!x) throw new Error('assertion: ' + (m || '')); }}\n\
             function eq(a, b, m) {{ if (a !== b) throw new Error((m || '') + ': ' + String(a) + ' !== ' + String(b)); }}\n\
             {source}\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn own_keys_and_descriptors() {
    check(
        r#"
        function f(a, b) {}
        const g = (a) => a;
        const h = { h() {} }.h;
        eq(Object.getOwnPropertyNames(f).join(), 'length,name,arguments,caller,prototype', 'f keys (sloppy: legacy arguments/caller, as V8)');
        eq(Object.getOwnPropertyNames(g).join(), 'length,name', 'arrow keys');
        eq(Reflect.ownKeys(h).join(), 'length,name', 'method keys');
        eq(f.length, 2); eq(g.length, 1); eq(h.length, 0);
        eq(f.name, 'f'); eq(g.name, 'g'); eq(h.name, 'h');
        for (const fn of [f, g, h]) {
            for (const k of ['length', 'name']) {
                const d = Object.getOwnPropertyDescriptor(fn, k);
                assert(d && !d.writable && !d.enumerable && d.configurable, k + ' descriptor');
            }
        }
        const p = Object.getOwnPropertyDescriptor(f, 'prototype');
        assert(p.writable && !p.enumerable && !p.configurable, 'prototype descriptor');
        eq(f.prototype.constructor, f, 'constructor backpointer');
        let seen = '';
        for (const k in f) seen += k;
        eq(seen, '', 'for-in sees nothing');
        assert(!('x' in g), 'no stray keys');
        "#,
    );
}

#[test]
fn writes_do_not_leak_between_closures() {
    check(
        r#"
        function mk() { return function inner(a, b, c) {}; }
        const a = mk(), b = mk();
        a.x = 1;
        eq(a.x, 1); eq(b.x, undefined, 'sibling untouched');
        eq(Object.keys(a).join(), 'x');
        eq(Object.keys(b).join(), '');
        delete a.name;
        eq(a.name, '', 'deleted name falls back to Function.prototype.name');
        eq(b.name, 'inner', 'sibling keeps name');
        eq(Object.getOwnPropertyNames(a).join(), 'length,arguments,caller,prototype,x');
        Object.defineProperty(b, 'length', { value: 7 });
        eq(b.length, 7); eq(a.length, 3); eq(mk().length, 3, 'fresh closure keeps template');
        const c = mk();
        c.length = 99;
        eq(c.length, 3, 'non-writable in sloppy mode');
        assert((() => { 'use strict'; try { c.length = 99; return false; } catch (e) { return e instanceof TypeError; } })(), 'strict throws');
        Object.defineProperty(c, 'name', { get() { return 'g'; } });
        eq(c.name, 'g'); eq(mk().name, 'inner');
        Object.freeze(c);
        assert(Object.isFrozen(c));
        "#,
    );
}

#[test]
fn name_inference_and_bind() {
    check(
        r#"
        const arrow = () => {};
        eq(arrow.name, 'arrow');
        const anon = function () {};
        eq(anon.name, 'anon');
        const named = function real() {};
        eq(named.name, 'real');
        const sym = Symbol('desc'), empty = Symbol(), o = {
            m() {}, get g() {}, set g(v) {}, [sym]: function () {}, [empty]: () => {}, arrow: () => {},
            ['comp' + 'uted']: function () {},
        };
        eq(o.m.name, 'm');
        const gd = Object.getOwnPropertyDescriptor(o, 'g');
        eq(gd.get.name, 'get g'); eq(gd.set.name, 'set g');
        eq(o[sym].name, '[desc]'); eq(o[empty].name, '');
        eq(o.arrow.name, 'arrow'); eq(o.computed.name, 'computed');
        let a1, a2;
        a1 = () => {}; eq(a1.name, 'a1');
        ({ a2 = () => {} } = {}); eq(a2.name, 'a2');
        function two(x, y) { return this; }
        const bound = two.bind(1, 2);
        eq(bound.name, 'bound two'); eq(bound.length, 1);
        eq(Object.getOwnPropertyNames(bound).join(), 'length,name');
        assert(!('prototype' in bound));
        eq(bound.bind().name, 'bound bound two');
        eq(bound.bind(null, 3, 4, 5).length, 0);
        class C { static name() { return 'sn'; } static length = 4; }
        eq(typeof C.name, 'function'); eq(C.length, 4);
        class D { static x = 1; }
        eq(D.name, 'D'); eq(Object.getOwnPropertyNames(D).join(), 'length,name,prototype,x');
        class E extends D {}
        eq(E.name, 'E');
        const F = class {}; eq(F.name, 'F');
        eq(String(arrow), '() => {}', 'toString unaffected');
        "#,
    );
}
