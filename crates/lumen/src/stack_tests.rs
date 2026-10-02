//! Native stack exhaustion on small thread stacks: deep recursion must surface as a catchable
//! error, never a process abort.

use crate::{Completion, Engine};

const SMALL_STACK: usize = 2 * 1024 * 1024;

/// Run `src` on a fresh engine on a default-sized (2 MiB) thread, returning `name: message` of
/// the thrown error or the completion value.
fn on_small_stack(src: &'static str) -> String {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || match Engine::new().eval(src, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, message }) => format!("{name}: {message}"),
            Err(e) => format!("ParseError: {}", e.message),
        })
        .unwrap()
        .join()
        .unwrap()
}

const OVERFLOW: &str = "RangeError: Maximum call stack size exceeded";

#[test]
fn deep_recursion_on_small_stack_throws_range_error() {
    assert_eq!(
        on_small_stack("function f(n) { return f(n + 1) + 1; } f(0)"),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack(
            "function f(n) { return n === 0 ? 0 : f(n - 1) + 1; }
             for (let k = 0; k < 200; k++) f(50);
             try { f(1e6); } catch (e) { String(e instanceof RangeError) }"
        ),
        "true"
    );
    assert_eq!(
        on_small_stack("function C() { new C(); } new C()"),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack("const a = [1]; function g() { return a.map(g); } g()"),
        OVERFLOW
    );
}

#[test]
fn deep_recursion_of_hot_function_on_small_stack() {
    assert_eq!(
        on_small_stack(
            "function f(n) { return n === 0 ? 0 : f(n - 1) + 1; }
             let s = 0;
             for (let k = 0; k < 20000; k++) s += f(3);
             try { f(1e6); 'no' } catch (e) { e.name + ':' + s }"
        ),
        "RangeError:60000"
    );
}

#[test]
fn deep_recursion_recovers_and_continues() {
    assert_eq!(
        on_small_stack(
            "function f(n) { return f(n + 1) + 1; }
             let c = 0;
             for (let k = 0; k < 3; k++) { try { f(0); } catch (e) { c++; } }
             function g(n) { return n ? g(n - 1) + 1 : 0; }
             c + ':' + g(50)"
        ),
        "3:50"
    );
}

#[test]
fn json_parse_deep_nesting_throws() {
    assert_eq!(
        on_small_stack("JSON.parse('['.repeat(3e6) + ']'.repeat(3e6))"),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack("JSON.parse('{\"a\":'.repeat(1e6) + '1' + '}'.repeat(1e6))"),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack("JSON.parse('['.repeat(1e6) + ']'.repeat(1e6), (k, v) => v)"),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack("JSON.rawJSON('['.repeat(1e6) + ']'.repeat(1e6))"),
        "SyntaxError: JSON.rawJSON value must be a primitive"
    );
    assert_eq!(
        on_small_stack("JSON.stringify(JSON.parse('[[[1]]]'))"),
        "[[[1]]]"
    );
}

#[test]
fn json_stringify_deep_nesting_throws() {
    assert_eq!(
        on_small_stack(
            "let a = []; for (let k = 0; k < 1e6; k++) a = [a];
             JSON.stringify(a)"
        ),
        OVERFLOW
    );
    assert_eq!(
        on_small_stack(
            "let o = {}; for (let k = 0; k < 1e6; k++) o = { o };
             JSON.stringify(o, null, 1)"
        ),
        OVERFLOW
    );
}

#[test]
fn json_stringify_cycles_still_detected() {
    assert_eq!(
        on_small_stack(
            "const a = []; let o = a; for (let k = 0; k < 100; k++) o = [o]; a.push(o);
             try { JSON.stringify(a); 'no' } catch (e) { e.name }"
        ),
        "TypeError"
    );
    assert_eq!(
        on_small_stack("const x = {}; JSON.stringify([x, x, { y: x }])"),
        "[{},{},{\"y\":{}}]"
    );
}

#[test]
fn regexp_deep_nesting_throws() {
    let r = on_small_stack("new RegExp('('.repeat(1e5) + 'a' + ')'.repeat(1e5))");
    assert!(
        r.starts_with("SyntaxError") || r == OVERFLOW,
        "unexpected: {r}"
    );
    let r = on_small_stack("new RegExp('(?:'.repeat(1e5) + 'a' + ')'.repeat(1e5))");
    assert!(
        r.starts_with("SyntaxError") || r == OVERFLOW,
        "unexpected: {r}"
    );
    let r = on_small_stack("new RegExp('[' + '[a'.repeat(1e5) + ']'.repeat(1e5) + ']', 'v')");
    assert!(
        r.starts_with("SyntaxError") || r == OVERFLOW,
        "unexpected: {r}"
    );
    assert_eq!(
        on_small_stack("String(new RegExp('('.repeat(50) + 'a' + ')'.repeat(50)).test('a'))"),
        "true"
    );
}

#[test]
fn regexp_deep_match_on_small_stack() {
    assert_eq!(
        on_small_stack(
            "const r = [];
             for (const n of [100, 1400, 1490, 3000]) r.push(/(a|b)*c/.test('ab'.repeat(n) + 'c'));
             r.push(new RegExp('('.repeat(50) + 'a*' + ')'.repeat(50) + 'b').test('a'.repeat(2000) + 'b'));
             r.join()"
        ),
        "true,true,true,true,true"
    );
}

#[test]
fn deep_expression_nesting_on_small_stack() {
    let r = on_small_stack("eval('('.repeat(1e5) + '1' + ')'.repeat(1e5))");
    assert!(
        r.starts_with("SyntaxError") || r == OVERFLOW,
        "unexpected: {r}"
    );
}

#[test]
fn deep_statement_nesting_on_small_stack() {
    let r = on_small_stack("eval('{'.repeat(1e5) + '}'.repeat(1e5))");
    assert!(
        r.starts_with("SyntaxError") || r == OVERFLOW,
        "unexpected: {r}"
    );
}

#[test]
fn deeply_nested_array_frees_without_overflow() {
    assert_eq!(
        on_small_stack("let a = []; for (let k = 0; k < 1e6; k++) a = [a]; a = null; 1"),
        "1"
    );
    assert_eq!(
        on_small_stack("let o = {}; for (let k = 0; k < 1e6; k++) o = { o }; 2"),
        "2"
    );
}

#[test]
fn proxy_chain_internal_methods_throw() {
    let ops = [
        "p.x",
        "'x' in p",
        "p.x = 1",
        "delete p.x",
        "Object.keys(p)",
        "Object.getPrototypeOf(p)",
        "Object.setPrototypeOf(p, null)",
        "Object.defineProperty(p, 'x', { value: 1 })",
        "Object.getOwnPropertyDescriptor(p, 'x')",
        "Object.isFrozen(p)",
        "Object.isExtensible(p)",
        "Object.preventExtensions(p)",
        "JSON.stringify(p)",
        "Object.create(p).x",
        "Object.create(p) instanceof Array",
        "for (var k in p);",
        "({ ...p })",
    ];
    for op in ops {
        let src: &'static str = Box::leak(
            format!(
                "let p = {{}}; for (let i = 0; i < 1e5; i++) p = new Proxy(p, {{}}); \
                 try {{ {op}; 'no error' }} catch (e) {{ e.name + ': ' + e.message }}"
            )
            .into_boxed_str(),
        );
        assert_eq!(on_small_stack(src), OVERFLOW, "{op}");
    }
}

#[test]
fn proxy_chain_is_array_is_iterative() {
    assert_eq!(
        on_small_stack(
            "let p = []; for (let i = 0; i < 1e5; i++) p = new Proxy(p, {}); \
             String(Array.isArray(p))"
        ),
        "true"
    );
}

#[test]
fn proxied_function_chain_call_and_construct_throw() {
    let base = "let q = function () {}; for (let i = 0; i < 1e5; i++) q = new Proxy(q, {});";
    for op in ["q()", "new q()"] {
        let src: &'static str = Box::leak(
            format!("{base} try {{ {op}; 'no error' }} catch (e) {{ e.name + ': ' + e.message }}")
                .into_boxed_str(),
        );
        assert_eq!(on_small_stack(src), OVERFLOW, "{op}");
    }
}

#[test]
fn deep_array_flat_throws() {
    assert_eq!(
        on_small_stack(
            "let d = []; for (let i = 0; i < 1e5; i++) d = [d]; \
             try { d.flat(Infinity); 'no error' } catch (e) { e.name + ': ' + e.message }"
        ),
        OVERFLOW
    );
}

#[test]
fn bound_function_chain_throws() {
    assert_eq!(
        on_small_stack(
            "let f = function () {}; for (let i = 0; i < 1e5; i++) f = f.bind(null); \
             let r = []; for (const op of [() => f(), () => new f()]) \
             try { op(); r.push('no error') } catch (e) { r.push(e.name) } r.join()"
        ),
        "RangeError,RangeError"
    );
}

#[test]
fn class_extends_chain_on_small_stack() {
    assert_eq!(
        on_small_stack(
            "let C = class {}; for (let i = 0; i < 20000; i++) C = class extends C {}; \
             try { new C(); 'ok' } catch (e) { e.name + ': ' + e.message }"
        ),
        OVERFLOW
    );
}

/// "bound bound … f" names of a bind chain share one buffer: memory linear in the chain.
#[test]
fn bound_name_chain_memory_is_linear() {
    let out = std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(|| {
            let mut e = Engine::new();
            let r = e.eval(
                "let f = function f() {}; for (let i = 0; i < 20000; i++) f = f.bind(null); \
                 f.name.length + ' ' + f.name.startsWith('bound bound ') + ' ' + f.name.endsWith(' f')",
                false,
            );
            let (_, _, root_bytes) = crate::lstr::view_stats();
            drop(e);
            (r, root_bytes)
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(matches!(out.0, Ok(Completion::Value(ref v)) if v == "120001 true true"));
    assert!(out.1 < 4 << 20, "bound names hold {} root bytes", out.1);
}
