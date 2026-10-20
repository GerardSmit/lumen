//! Early errors inside function bodies the parser skips must still be reported at load, whether
//! the syntax-only body check or the full parse finds them.

fn wrapped(body: &str) -> Vec<String> {
    [
        format!("function f(){{{body}\n}}"),
        format!("function f(){{'use strict';{body}\n}}"),
        format!("function* f(){{{body}\n}}"),
        format!("async function f(){{{body}\n}}"),
        format!("var f=()=>{{{body}\n}};"),
        format!("var o={{m(){{{body}\n}}}};"),
        format!("class C{{m(){{{body}\n}}}}"),
    ]
    .into()
}

fn rejects(body: &str) {
    for src in wrapped(body) {
        assert!(lumen::check_script_syntax(&src).is_err(), "accepted: {src}");
    }
}

fn accepts(body: &str) {
    for src in wrapped(body) {
        if let Err(e) = lumen::check_script_syntax(&src) {
            panic!("rejected {src}: {}", e.message);
        }
    }
}

#[test]
fn redeclared_lexical_names() {
    rejects("let a; let a;");
    rejects("const a = 1; var a;");
    rejects("let a; { var a; }");
    rejects("function g(){} let g;");
}

#[test]
fn bad_targets_and_operators() {
    rejects("1 = 2;");
    rejects("a ?? b || c;");
    rejects("a++ ++;");
    rejects("for (let x of y) { break nope; }");
    rejects("continue;");
    rejects("x: x: ;");
}

#[test]
fn bad_literals() {
    rejects("/(/;");
    rejects("`${`;");
    rejects("0777n;");
}

#[test]
fn undeclared_private_names_and_super() {
    rejects("this.#nope;");
    rejects("({ m() { super(); } });");
}

#[test]
fn valid_bodies_pass() {
    accepts("var a = 1; { let a = 2; } a++; return a;");
    accepts("for (const [k, v] of Object.entries(o)) { if (!v) continue; }");
    accepts("try { x(); } catch ({ message }) { y(message); } finally { z(); }");
    accepts("var r = /[a-z]+/giu.test(s) ? `a${b}c` : a ?? (b || c);");
    accepts("l: for (;;) { switch (k) { case 1: break l; default: continue l; } }");
    accepts("var { a, ...rest } = o; ({ a, ...rest } = o); [a, b] = [b, a];");
}

#[test]
fn deleting_private_members_through_optional_chains() {
    for src in [
        "class C{#x; m(){ delete this?.#x; }}",
        "class C{#x; m(){ delete this?.b.#x; }}",
        "class C{#x; m(){ delete this.#x; }}",
    ] {
        assert!(lumen::check_script_syntax(src).is_err(), "accepted: {src}");
    }
    assert!(lumen::check_script_syntax("class C{#x; m(){ delete this?.y; }}").is_ok());
}
