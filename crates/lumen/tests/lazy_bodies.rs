//! Function bodies are skipped at parse time and parsed on first call (plan 0223). A body that is
//! never called is never parsed — a syntax error in it is not a load-time error — and one that is
//! called sees exactly the parse context it was written in: strictness, generator/async, `super`
//! and `new.target` validity, private names, closures over the enclosing scope. A parsed body
//! that no call touched for a whole collection interval is released by the collector and parsed
//! again on the next call (the tests under "released cold bodies").
use lumen::{bytecode::Tier, Completion, Engine};

fn run_with(tier: Tier, source: &str) -> Completion {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    let script =
        format!("function assert(x, m) {{ if (!x) throw new Error(m || 'assertion'); }}\n{source}");
    engine
        .eval(&script, false)
        .unwrap_or_else(|e| panic!("{tier:?}: parse error: {}", e.message))
}

fn check_with(tier: Tier, source: &str) {
    match run_with(tier, &format!("{source}\n'passed'")) {
        Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
        Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
    }
}

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        check_with(tier, source);
    }
}

/// The tests that assert an error is *deferred* to the first call do not apply when every body
/// is parsed at load (`LUMEN_EAGER_PARSE=1`, the test262 configuration).
fn bodies_are_lazy() -> bool {
    !std::env::var_os("LUMEN_EAGER_PARSE").is_some_and(|v| !v.is_empty() && v != "0")
}

fn throws(source: &str) -> (String, String) {
    let mut last = None;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        match run_with(tier, source) {
            Completion::Throw { name, message } => last = Some((name, message)),
            Completion::Value(v) => panic!("{tier:?}: expected a throw, got {v}"),
        }
    }
    last.unwrap()
}

#[test]
fn a_syntax_error_in_an_uncalled_body_is_not_a_load_error() {
    if !bodies_are_lazy() {
        return;
    }
    check(
        r#"
        function never() { var 1 = 2; ) }
        const arrow = () => { class { } ; ( };
        class C { m() { let let = 1; } static s() { return ; ; ]; } get g() { !!! } }
        const o = { m() { for (;;) { } , } };
        assert(typeof never === "function");
        assert(typeof arrow === "function");
        assert(typeof C.prototype.m === "function");
        assert(typeof o.m === "function");
        "#,
    );
}

#[test]
fn calling_a_body_that_does_not_parse_throws_a_syntax_error_every_time() {
    if !bodies_are_lazy() {
        return;
    }
    let (name, message) = throws(
        r#"
        function bad() {
          return 1;
          var 1;
        }
        let first, second;
        try { bad(); } catch (e) { first = e; }
        try { bad(); } catch (e) { second = e; }
        assert(first instanceof SyntaxError, "first call");
        assert(second instanceof SyntaxError, "second call");
        assert(first.message === second.message, "same message");
        throw first;
        "#,
    );
    assert_eq!(name, "SyntaxError");
    assert!(message.contains("binding identifier"), "{message}");
}

#[test]
fn nested_functions_stay_lazy_until_their_own_first_call() {
    if !bodies_are_lazy() {
        return;
    }
    check(
        r#"
        function outer(x) {
          function inner(y) {
            const deeper = (z) => { return x + y + z; };
            return deeper(3);
          }
          function broken() { ((( }
          return inner(2);
        }
        assert(outer(1) === 6);
        assert(outer(10) === 15);
        "#,
    );
}

#[test]
fn generator_async_arrow_and_method_bodies_keep_their_context() {
    check(
        r#"
        function* gen() { yield 1; const inner = function* () { yield 2; }; yield* inner(); }
        assert([...gen()].join() === "1,2");
        async function af(x) { const v = await x; return v + 1; }
        let got;
        af(41).then(v => { got = v; });
        const arrow = (a, b) => { const sum = a + b; return sum * 2; };
        assert(arrow(1, 2) === 6);
        const asyncArrow = async (a) => { return (await a) + 1; };
        let got2;
        asyncArrow(1).then(v => { got2 = v; });
        class Base { constructor() { this.tag = "base"; } who() { return "Base"; } static make() { return new this(); } }
        class Derived extends Base {
          #secret = 7;
          constructor() { super(); this.tag += "+derived"; }
          who() { return "Derived<" + super.who() + ">"; }
          get secret() { return this.#secret; }
          set secret(v) { this.#secret = v; }
          static create() { return new.target === undefined ? Derived.make() : null; }
          *items() { yield this.#secret; yield #secret in this; }
          async later() { return await this.#secret; }
        }
        const d = Derived.create();
        assert(d.tag === "base+derived", d.tag);
        assert(d.who() === "Derived<Base>");
        assert(d.secret === 7);
        d.secret = 9;
        assert([...d.items()].join() === "9,true");
        function nt() { return new.target; }
        assert(nt() === undefined && new nt() !== undefined);
        const o = { m() { return this.v; }, v: 3, get g() { return this.v + 1; } };
        assert(o.m() === 3 && o.g === 4);
        "#,
    );
}

#[test]
fn async_results_arrive_after_the_body_parses_on_first_call() {
    check(
        r#"
        async function af(x) { const v = await x; return v + 1; }
        let got;
        af(41).then(v => { got = v; });
        Promise.resolve().then(() => {}).then(() => { assert(got === 42, "async result " + got); });
        "#,
    );
}

#[test]
fn a_use_strict_prologue_inside_a_lazy_body_governs_it() {
    check(
        r#"
        function strictBody() { "use strict"; undeclaredName = 1; }
        let threw = false;
        try { strictBody(); } catch (e) { threw = e instanceof ReferenceError; }
        assert(threw, "assignment to an undeclared name is a ReferenceError in strict code");
        function sloppyBody() { anotherUndeclared = 2; return anotherUndeclared; }
        assert(sloppyBody() === 2);
        function strictThis() { "use strict"; return this; }
        assert(strictThis() === undefined);
        function sloppyThis() { return this; }
        assert(sloppyThis() === globalThis);
        "#,
    );
}

#[test]
fn a_use_strict_prologue_makes_strict_only_syntax_an_error_at_first_call() {
    if !bodies_are_lazy() {
        return;
    }
    let (name, message) = throws(
        r#"
        function f() { "use strict"; with ({}) {} }
        f();
        "#,
    );
    assert_eq!(name, "SyntaxError");
    assert!(message.contains("with"), "{message}");
}

#[test]
fn a_body_lexical_clashing_with_a_parameter_errors_at_first_call() {
    if !bodies_are_lazy() {
        return;
    }
    let (name, message) = throws(
        r#"
        function f(a) { let a = 1; return a; }
        assert(typeof f === "function", "declared fine");
        f(1);
        "#,
    );
    assert_eq!(name, "SyntaxError");
    assert!(
        message.contains("'a' has already been declared"),
        "{message}"
    );
}

#[test]
fn braces_inside_strings_templates_regexes_and_comments_do_not_end_the_body() {
    check(
        r#"
        function f(x) {
          const s = "}" + '{' + "\"}";
          const t = `}${x}{${`}${x}`}`;
          const r = /}{/.source + /[}]/.exec("}")[0];
          // }
          /* } { */
          const div = 4 / 2 / 1;
          return s + t + r + div;
        }
        assert(f(1) === '}{"}}1{}1}{}2', f(1));
        function g() { return { a: { b: 1 } }.a.b; }
        assert(g() === 1);
        "#,
    );
}

#[test]
fn to_string_is_the_original_source_text() {
    check(
        r#"
        function  f ( a ,b ) { /* keep */ return a+b } // tail
        assert(f.toString() === "function  f ( a ,b ) { /* keep */ return a+b }", f.toString());
        f(1, 2);
        assert(f.toString() === "function  f ( a ,b ) { /* keep */ return a+b }", "after the call");
        const arrow = (x) => { return x };
        assert(arrow.toString() === "(x) => { return x }", arrow.toString());
        class C { m ( ) { return 1 } static s() {} }
        assert(C.prototype.m.toString() === "m ( ) { return 1 }", C.prototype.m.toString());
        assert(C.toString() === "class C { m ( ) { return 1 } static s() {} }", C.toString());
        const o = { get  g() { return 2 } };
        assert(Object.getOwnPropertyDescriptor(o, "g").get.toString() === "get  g() { return 2 }");
        async function* ag() { yield 1 }
        assert(ag.toString() === "async function* ag() { yield 1 }");
        "#,
    );
}

#[test]
fn eval_inside_a_lazy_body_sees_its_scope() {
    check(
        r#"
        function f(x) {
          let local = 10;
          const r = eval("x + local");
          eval("var hoisted = 5");
          return r + hoisted;
        }
        assert(f(1) === 16, f(1));
        function g() { "use strict"; eval("var notLeaked = 1"); return typeof notLeaked; }
        assert(g() === "undefined");
        class C { #p = 3; m() { return eval("this.#p"); } }
        assert(new C().m() === 3);
        "#,
    );
}

#[test]
fn closures_capture_the_enclosing_scope() {
    check(
        r#"
        let counter = 0;
        function make(step) {
          let local = 0;
          return {
            inc() { local += step; counter += step; return local; },
            get() { return local; },
          };
        }
        const a = make(1), b = make(10);
        a.inc(); a.inc(); b.inc();
        assert(a.get() === 2 && b.get() === 10 && counter === 12);
        function loop() {
          const fns = [];
          for (let i = 0; i < 3; i++) fns.push(function () { return i; });
          return fns.map(f => f()).join();
        }
        assert(loop() === "0,1,2");
        "#,
    );
}

#[test]
fn private_names_used_inside_a_lazy_method_resolve() {
    check(
        r#"
        class A {
          #x = 1;
          static #count = 0;
          #m() { return this.#x + 1; }
          run() { A.#count++; return this.#m() + (#x in this ? 10 : 0); }
          static count() { return A.#count; }
          nested() {
            class B { #y = 5; get() { return this.#y; } }
            return new B().get() + this.#x;
          }
        }
        const a = new A();
        assert(a.run() === 12);
        assert(A.count() === 1);
        assert(a.nested() === 6);
        "#,
    );
}

#[test]
fn an_undeclared_private_name_inside_a_lazy_body_is_a_syntax_error_at_first_call() {
    if !bodies_are_lazy() {
        return;
    }
    let (name, message) = throws(
        r#"
        class A { m() { return this.#nope; } }
        new A().m();
        "#,
    );
    assert_eq!(name, "SyntaxError");
    assert!(message.contains("#nope"), "{message}");
}

#[test]
fn super_and_arguments_context_errors_are_still_reported() {
    if !bodies_are_lazy() {
        return;
    }
    // A super call outside a derived constructor, through an arrow, at the arrow's first call.
    let (name, _) = throws(
        r#"
        class A { m() { const f = () => { super(); }; f(); } }
        new A().m();
        "#,
    );
    assert_eq!(name, "SyntaxError");
    // `arguments` in a field initializer, through an arrow whose body is lazy.
    let (name, message) = throws(
        r#"
        class A { f = () => { return arguments; }; }
        new A().f();
        "#,
    );
    assert_eq!(name, "SyntaxError");
    assert!(message.contains("field initializer"), "{message}");
    // `new.target` inside an arrow at the top level has no enclosing function.
    let (name, _) = throws(
        r#"
        const f = () => { return new.target; };
        f();
        "#,
    );
    assert_eq!(name, "SyntaxError");
}

#[test]
fn iifes_run_at_load_and_report_their_errors_at_load() {
    check(
        r#"
        var ran = [];
        (function () { ran.push("paren"); })();
        (function () { ran.push("call-inside"); }());
        !function () { ran.push("bang"); }();
        (() => { ran.push("arrow"); })();
        (function () { ran.push("call-method"); }).call(null);
        assert(ran.join() === "paren,call-inside,bang,arrow,call-method", ran.join());
        "#,
    );
    let mut engine = Engine::new();
    assert!(engine.eval("(function () { var 1; })();", false).is_err());
}

#[test]
fn the_function_constructor_reports_a_bad_body_at_creation() {
    check(
        r#"
        let threw = false;
        try { new Function("a", "var 1;"); } catch (e) { threw = e instanceof SyntaxError; }
        assert(threw, "creation throws");
        assert(new Function("a", "b", "return a * b")(6, 7) === 42);
        assert(new Function("a", "b", "return a * b").toString() === "function anonymous(a,b\n) {\nreturn a * b\n}");
        "#,
    );
}

#[test]
fn a_module_with_lazy_bodies_runs_them_on_call() {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Bytecode);
    engine.set_tier_threshold(0);
    let src = r#"
        export function f(x) { const y = x * 2; return y + 1; }
        const g = (a) => { let b = a; return b; };
        if (f(1) !== 3 || g(4) !== 4) throw new Error("wrong");
        globalThis.moduleResult = "passed";
    "#;
    match engine.eval_module(src, "lazy-mod.mjs", |_, _| None) {
        Ok(_) => {}
        Err(e) => panic!("{e:?}"),
    }
    match engine.eval("moduleResult", false).unwrap() {
        Completion::Value(v) => assert_eq!(v, "passed"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn a_snapshot_runs_its_functions() {
    let src = "function f(x) { function g() { return x + 1; } return g(); }\nf(1);";
    let blob = lumen::compile_snapshot(src).unwrap();
    let mut engine = Engine::new();
    match engine.eval_snapshot(&blob, src, false).unwrap() {
        Completion::Value(v) => assert_eq!(v, "2"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn multibyte_text_before_and_inside_a_lazy_body_slices_exactly() {
    // Lazy ranges are byte offsets into the file, mapped from the char offsets the tokens
    // carry; multibyte chars before, inside and after the body must not shift either end,
    // and a nested lazy body (lexed from its own slice) must map back to the whole file.
    check(
        r#"
        const before = "héllo — wörld ✓ 日本語 🎉";
        function  f ( a ) { // ünïcode → “quotes”
          const s = "ß→∞";
          const inner = ( b ) => { return b + "™" + s + "😀" };
          return inner(a) + "€";
        }
        const tail = "après";
        assert(f.toString() === 'function  f ( a ) { // ünïcode → “quotes”\n          const s = "ß→∞";\n          const inner = ( b ) => { return b + "™" + s + "😀" };\n          return inner(a) + "€";\n        }', f.toString());
        assert(f("x") === "x™ß→∞😀€", f("x"));
        assert(f.toString().endsWith('"€";\n        }'), "after the call");
        const inner = ( b ) => { return b + "™" };
        assert(inner.toString() === '( b ) => { return b + "™" }', inner.toString());
        class Ç { mé ( ) { return "é" } }
        assert(Ç.toString() === 'class Ç { mé ( ) { return "é" } }', Ç.toString());
        assert(Ç.prototype.mé.toString() === 'mé ( ) { return "é" }');
        assert(new Ç().mé() === "é");
        "#,
    );
}

#[test]
fn nested_lazy_functions_inside_a_dynamic_function_have_their_source() {
    check(
        r#"
        const f = new Function("a", "const g = function  named ( b ) { return a + b } ; const h = ( c ) => { return c * 2 } ; return [g, h];");
        const [g, h] = f(1);
        assert(g.toString() === "function  named ( b ) { return a + b }", g.toString());
        assert(h.toString() === "( c ) => { return c * 2 }", h.toString());
        assert(g(2) === 3 && h(4) === 8);
        assert(g.toString() === "function  named ( b ) { return a + b }", "after the call");
        const u = new Function("s", "const k = ( ) => { return s + 'ü' } ; return k;");
        assert(u("é").toString() === "( ) => { return s + 'ü' }", u("é").toString());
        assert(u("é")() === "éü");
        "#,
    );
}

// ----- released cold bodies (the collector flushes a body no call touched for a whole collection
// interval and re-parses it on the next call; `$262.gc()` forces a collection) -----

#[test]
fn a_function_called_once_then_flushed_returns_the_same_result_when_called_again() {
    check(
        "function f(a, b) { const s = a + b; return [s, a * b].join(','); }
         const before = f(2, 3);
         $262.gc(); $262.gc();
         assert(f(2, 3) === before, 'same result after the flush');
         $262.gc(); $262.gc(); $262.gc();
         assert(f(4, 5) === '9,20', 'and again');",
    );
}

#[test]
fn a_generator_suspended_inside_a_flushed_body_resumes_and_completes() {
    check(
        "function* g(n) { let acc = 0; for (let i = 1; i <= n; i++) { acc += yield i; } return acc; }
         const it = g(3);
         assert(it.next().value === 1, 'first');
         $262.gc(); $262.gc(); $262.gc();
         assert(it.next(10).value === 2, 'second');
         $262.gc(); $262.gc();
         assert(it.next(20).value === 3, 'third');
         const last = it.next(30);
         assert(last.done && last.value === 60, 'return value');
         const it2 = g(1);
         it2.next();
         assert(it2.next(5).value === 5, 'a fresh generator after the flush');",
    );
}

#[test]
fn an_async_function_awaiting_across_a_flush_completes() {
    check(
        r#"
        async function a(x) { const y = await x; $262.gc(); $262.gc(); return y + 1; }
        let out = '';
        a(1).then(v => { out = String(v); });
        a(5).then(v => { out += ',' + v; });
        Promise.resolve().then(() => {}).then(() => {}).then(() => {}).then(() => {
            assert(out === '2,6', 'async results after the flush: ' + out);
        });
        "#,
    );
}

#[test]
fn a_collection_inside_a_recursive_call_keeps_every_active_frame_running() {
    check(
        "function r(n) {
           if (n === 0) { $262.gc(); $262.gc(); $262.gc(); return 0; }
           const below = r(n - 1);
           return below + 1;
         }
         assert(r(5) === 5, 'frames above the collection finish on their own copy');
         assert(r(3) === 3, 'a later call re-parses');",
    );
}

#[test]
fn a_hoisted_nested_declaration_resolves_after_a_flush() {
    check(
        "function outer(x) {
           var v = helper(x);
           return v + tail();
           function helper(k) { return k * 2; }
           function tail() { return '!'; }
         }
         assert(outer(2) === '4!', 'first');
         $262.gc(); $262.gc();
         assert(outer(3) === '6!', 'after the flush');
         const keep = (function () { return function held() { return 'held'; }; })();
         $262.gc(); $262.gc();
         assert(keep() === 'held', 'a closure created before the flush');",
    );
}

#[test]
fn to_string_is_unchanged_by_a_flush() {
    check(
        "function  f ( a ) { /* c */ return a ; }
         const before = f.toString();
         f(1);
         $262.gc(); $262.gc();
         assert(f.toString() === before, 'same text');
         f(2);
         assert(f.toString() === before, 'and after the re-parse');",
    );
}

#[test]
fn a_flushed_class_method_still_reaches_super_and_private_names() {
    check(
        "class A { m() { return 'a'; } static s() { return 'S'; } }
         class B extends A {
           #p = 1;
           static #q = 2;
           m() { return super.m() + this.#p; }
           static s() { return super.s() + B.#q; }
           get g() { return this.#p + 10; }
         }
         const b = new B();
         assert(b.m() === 'a1' && B.s() === 'S2' && b.g === 11, 'first');
         $262.gc(); $262.gc();
         assert(b.m() === 'a1', 'super and a private field after the flush');
         assert(B.s() === 'S2', 'static super and a static private field');
         assert(b.g === 11, 'an accessor');
         $262.gc(); $262.gc();
         assert(new B().m() === 'a1', 'a new instance');",
    );
}

#[test]
fn a_body_that_does_not_parse_keeps_throwing_after_collections() {
    if !bodies_are_lazy() {
        return;
    }
    check(
        "function bad() { var 1; }
         function probe() { try { bad(); } catch (e) { return e.constructor === SyntaxError; } return false; }
         assert(probe(), 'first call');
         $262.gc(); $262.gc();
         assert(probe(), 'after collections');",
    );
}

#[test]
fn a_tagged_template_site_keeps_its_strings_object_across_a_flush() {
    check(
        "function tag(s) { return s; }
         function t(x) { return tag`a${x}b`; }
         const first = t(1);
         $262.gc(); $262.gc();
         const second = t(2);
         assert(first === second, 'GetTemplateObject is per site, and the site survived the flush');
         assert(first.raw[0] === 'a' && first[1] === 'b', 'contents');
         function u() { return tag`a${0}b`; }
         assert(u() !== first, 'a different site with the same text is a different object');",
    );
}

#[test]
fn many_flushes_in_a_loop_keep_calling_correctly() {
    check(
        "function f(i) { return i * 2; }
         let sum = 0;
         for (let i = 0; i < 6; i++) { sum += f(i); $262.gc(); $262.gc(); }
         assert(sum === 30, 'sum');",
    );
}
