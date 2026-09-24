//! `strip_types` against Node's `module.stripTypeScriptTypes` (v24, amaro/swc strip-only mode).
//! The expected outputs were produced by Node itself; a larger differential corpus and a
//! real-world check against Node live outside the crate (see `lumen typed --strip-cases`, crates/lumen-cli/src/typed.rs).

use super::{side_table_for, strip_types, CJS_PARAMS, INVALID, UNSUPPORTED};

const STRIP_CASES: &[(&str, &str)] = &[
    (
        "let x: number = 1;",
        "let x         = 1;",
    ),
    (
        "let a!: string;",
        "let a         ;",
    ),
    (
        "function f(a: number, b?: string, ...c: any[]): void {}",
        "function f(a        , b         , ...c       )       {}",
    ),
    (
        "function f(this: Window, a: number) {}",
        "function f(              a        ) {}",
    ),
    (
        "const f = (a: number): string => String(a);",
        "const f = (a        )         => String(a);",
    ),
    (
        "const f = async <T,>(a: T): Promise<T> => a;",
        "const f = async     (a   )             => a;",
    ),
    (
        "function f<T extends object = {}>(a: T): asserts a is T {}",
        "function f                       (a   )                 {}",
    ),
    (
        "function g(x: unknown): x is string { return true; }",
        "function g(x         )              { return true; }",
    ),
    (
        "try {} catch (e: unknown) {}",
        "try {} catch (e         ) {}",
    ),
    (
        "for (let i: number = 0; i < 1; i++) {}",
        "for (let i         = 0; i < 1; i++) {}",
    ),
    (
        "let { a, b }: { a: 1; b: 2 } = o;",
        "let { a, b }                 = o;",
    ),
    (
        "let y = x as unknown as string;",
        "let y = x                     ;",
    ),
    (
        "let y = x satisfies T;",
        "let y = x            ;",
    ),
    (
        "let y = x!.y!;",
        "let y = x .y ;",
    ),
    (
        "let y = f<string>(1);",
        "let y = f        (1);",
    ),
    (
        "let y = new Map<string, number>();",
        "let y = new Map                ();",
    ),
    (
        "let y = f<T>;",
        "let y = f   ;",
    ),
    (
        "let y = a?.b!<T>();",
        "let y = a?.b    ();",
    ),
    (
        "let y = a < b > c;",
        "let y = a < b > c;",
    ),
    (
        "f(a < b, c > (d));",
        "f(a          (d));",
    ),
    (
        "let t = f<T>`x`;",
        "let t = f   `x`;",
    ),
    (
        "type A = { x: number };\nlet y = 1;",
        "                       \nlet y = 1;",
    ),
    (
        "interface I extends J { m(): void }\nexport interface K {}",
        "                                   \n                     ",
    ),
    (
        "declare const x: number;\ndeclare function f(): void;\ndeclare class C {}",
        "                        \n                           \n                  ",
    ),
    (
        "declare module 'x' { export const a: 1 }",
        "                                        ",
    ),
    (
        "declare global { interface W {} }",
        "                                 ",
    ),
    (
        "declare namespace N { const x: 1 }",
        "                                  ",
    ),
    (
        "namespace N { export type X = 1 }",
        "                                 ",
    ),
    (
        "declare enum E { A }",
        "                    ",
    ),
    (
        "import type { A } from 'a';\nimport { type B, C } from 'b';",
        "                           \nimport {         C } from 'b';",
    ),
    (
        "import { type A as B } from 'a';",
        "import {             } from 'a';",
    ),
    (
        "import { a as b } from 'a';",
        "import { a as b } from 'a';",
    ),
    (
        "export type { A } from 'a';\nexport { type B, c };",
        "                           \nexport {         c };",
    ),
    (
        "import type x = require('x');",
        "                             ",
    ),
    (
        "export as namespace X;",
        "export as namespace X;",
    ),
    (
        "function f(a: number): void;\nfunction f(a: any) {}",
        "                            \nfunction f(a     ) {}",
    ),
    (
        "class A<T> extends B<T> implements C, D { x: number = 1; private y?: string; static z: T; }",
        "class A    extends B                    { x         = 1;         y         ; static z   ; }",
    ),
    (
        "abstract class A { abstract m(): void; protected abstract x: number; }",
        "         class A {                                                   }",
    ),
    (
        "class A { declare x: number; [k: string]: any; constructor() {} }",
        "class A {                                      constructor() {} }",
    ),
    (
        "class A extends B { public m(): void {} private get g(): number { return 1 } readonly r = 1; override o() {} }",
        "class A extends B {        m()       {}         get g()         { return 1 }          r = 1;          o() {} }",
    ),
    (
        "class A { m(): void; m(a?: any) {} }",
        "class A {            m(a      ) {} }",
    ),
    (
        "class A { accessor x: number = 1; static accessor y = 2; }",
        "class A { accessor x         = 1; static accessor y = 2; }",
    ),
    (
        "export default abstract class {}",
        "export default          class {}",
    ),
    (
        "class A { x = 1\n  public [k] = 2 }",
        "class A { x = 1\n  ;      [k] = 2 }",
    ),
    (
        "class A { x = 1\n  private *g() {} }",
        "class A { x = 1\n  ;       *g() {} }",
    ),
    (
        "@dec class A { @dec m(@p x: number) {} }",
        "@dec class A { @dec m(@p x        ) {} }",
    ),
    (
        "let x = 1\ntype T = 1\n(a)",
        "let x = 1\n;         \n(a)",
    ),
    (
        "type A = 1;\ntype B = 2;\n(b)",
        "          ;\n           \n(b)",
    ),
    (
        "let x = y as T\n(a)",
        "let x = y ;   \n(a)",
    ),
    (
        "let x = 1\ninterface I {}\n[a]",
        "let x = 1\n;             \n[a]",
    ),
    (
        "if (a) type T = 1\nelse b()",
        "if (a) ;         \nelse b()",
    ),
    (
        "let x = y\ndeclare let z: 1\n`t`",
        "let x = y\n;               \n`t`",
    ),
    (
        "let x: { é: 1; 中: 2; '😀': 3 } = o;",
        "let x                  ﻿       = o;",
    ),
    (
        "let x: T /* é */ = 1;",
        "let x    /* é */ = 1;",
    ),
    (
        "let a = b ? (c) : d => e;",
        "let a = b ? (c) : d => e;",
    ),
    (
        "let a = b ? c : d;",
        "let a = b ? c : d;",
    ),
    (
        "let r = /[/]<T>/g.test(s), q = a / b / c;",
        "let r = /[/]<T>/g.test(s), q = a / b / c;",
    ),
    (
        "let s = `${a as any}` + 'type x = 1';",
        "let s = `${a       }` + 'type x = 1';",
    ),
    (
        "let type = 1, as = 2, satisfies = 3; type\n= 4;",
        "let type = 1, as = 2, satisfies = 3; type\n= 4;",
    ),
    (
        "label: for (;;) { break label; }",
        "label: for (;;) { break label; }",
    ),
    (
        "#!/usr/bin/env node\nlet x: number = 1;",
        "#!/usr/bin/env node\nlet x         = 1;",
    ),
    (
        "const f = (a): (b: number) => number => b => b;",
        "const f = (a)                        => b => b;",
    ),
    (
        "const f = (\n  a: number,\n): Promise<\n  void\n> => g();",
        "const f = (\n  a        ,\n           \n      \n) => g();",
    ),
];

const STRIP_ERRORS: &[(&str, &str, &str)] = &[
    (
        "enum E { A }",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript enum is not supported in strip-only mode",
    ),
    (
        "const enum E { A }",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript enum is not supported in strip-only mode",
    ),
    (
        "namespace N { export const x = 1 }",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript namespace declaration is not supported in strip-only mode",
    ),
    (
        "module N { type X = 1 }",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "`module` keyword is not supported. Use `namespace` instead.",
    ),
    (
        "class A { constructor(private x: number) {} }",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript parameter property is not supported in strip-only mode",
    ),
    (
        "import x = require('y');",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript import equals declaration is not supported in strip-only mode",
    ),
    (
        "export = x;",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "TypeScript export assignment is not supported in strip-only mode",
    ),
    (
        "let x = <number>y;",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        "The angle-bracket syntax for type assertions, `<T>expr`, is not supported in type strip mode. Instead, use the 'as' syntax: `expr as T`.",
    ),
    (
        "let x: = 1;",
        "ERR_INVALID_TYPESCRIPT_SYNTAX",
        "",
    ),
];

#[test]
fn strip_matches_node() {
    for &(src, want) in STRIP_CASES {
        match strip_types(src) {
            Ok(got) => assert_eq!(got, want, "\n  source: {src:?}"),
            Err(e) => panic!("{src:?}: {e}"),
        }
    }
}

#[test]
fn strip_rejects_like_node() {
    for &(src, code, message) in STRIP_ERRORS {
        let e = strip_types(src).expect_err(src);
        assert_eq!(e.code, Some(code), "{src:?}: {e}");
        if code == UNSUPPORTED {
            assert_eq!(e.message, message, "{src:?}");
        } else {
            assert_eq!(code, INVALID);
        }
    }
}

#[test]
fn strip_keeps_offsets() {
    for &(src, _) in STRIP_CASES {
        let out = strip_types(src).unwrap();
        assert_eq!(out.len(), src.len(), "{src:?}: UTF-8 length");
        assert_eq!(
            out.encode_utf16().count(),
            src.encode_utf16().count(),
            "{src:?}: UTF-16 length"
        );
        let lines = |s: &str| s.match_indices('\n').map(|m| m.0).collect::<Vec<_>>();
        assert_eq!(lines(&out), lines(src), "{src:?}: line breaks");
    }
}

#[test]
fn strip_error_position() {
    let e = strip_types("let a = 1;\n  enum E { A }").unwrap_err();
    assert_eq!((e.line, e.column, e.offset), (2, 3, 13));
    let e = strip_types("let x: = 1;").unwrap_err();
    assert_eq!(e.code, Some(INVALID));
}

#[test]
fn strip_plain_js_is_identity() {
    for src in [
        "let a = b < c, d = e > (f);",
        "a = b ? (c) : (d) => e;",
        "x = y / z / w; r = /=>/g;",
        "var type = 1, declare = 2, abstract = 3, namespace = 4, module = 5;",
        "type\n= 1; declare\nfunction f() {}",
        "class C { static x = 1; #p = 2; get [k]() { return 1 } }",
        "for await (const x of y) {} label: { break label; }",
        "let s = `a${`b${c}`}d`; let o = { enum: 1, as: 2, interface: 3 };",
    ] {
        assert_eq!(strip_types(src).unwrap(), src);
    }
}

#[test]
fn side_table_keys_are_file_offsets() {
    use crate::ast::FnSource;
    let src = "const n: number = 1;\nfunction add(a: number, b?: string): number { return a; }\n";
    let f = crate::parser::parse_cjs_function(src, &CJS_PARAMS, true).unwrap();
    let FnSource::Range { src: rc, .. } = &f.source else {
        panic!("no range")
    };
    // Blanked in place, same bytes.
    assert_eq!(rc.len(), src.len());
    assert!(!rc.contains("number"));
    let t = side_table_for(rc).expect("side table");
    let at = src.find("function add").unwrap() as u32;
    let sf = t.fn_at(at).expect("fn fact at the file offset");
    assert_eq!(sf.params.len(), 2);
    assert_eq!(t.ty(sf.params[0].ty.unwrap()), "number");
    assert!(sf.params[1].optional);
    assert_eq!(t.ty(sf.ret.unwrap()), "number");
    #[cfg(feature = "typed")]
    {
        let tt = super::type_table(rc).expect("type table");
        assert!(tt.fn_at(at).is_some());
        // Cached: the same table the second time.
        assert!(std::rc::Rc::ptr_eq(&tt, &super::type_table(rc).unwrap()));
    }
}

#[cfg(feature = "typed")]
#[test]
fn js_docs_are_recorded() {
    use crate::ast::FnSource;
    let src = "/** @param {number} a */\nfunction f(a) { return a; }\n";
    let f = crate::parser::parse_cjs_function(src, &CJS_PARAMS, false).unwrap();
    let FnSource::Range { src: rc, .. } = &f.source else {
        panic!("no range")
    };
    let t = side_table_for(rc).expect("side table");
    assert_eq!(t.docs, vec![(0, 24)]);
    let at = src.find("function").unwrap() as u32;
    assert!(t.doc_before(rc, at).is_some());
}

/// The `stack` text of the SyntaxError, as Node v24 builds it (its code frame), for rejected
/// sources: the expected strings are Node's own `e.stack` up to the first `at` frame.
#[test]
fn node_error_frames() {
    let frame = |src: &str| {
        let e = strip_types(src).unwrap_err();
        super::node_error_text("", src, Some((e.offset, e.end)), e.line, &e.message)
    };
    let head = |s: String| s.split("\n\nSyntaxError").next().unwrap().to_string();
    let cases = [
        ("const a = 1;\nenum E { A }\nconsole.log(a);\n", ":2\nconst a = 1;\nenum E { A }\n^^^^^^^^^^^^\nconsole.log(a);"),
        ("let a;\nenum E {\n  A,\n  B\n}\nlet b;\n", ":2\n    let a;\n  > enum E {\n      A,\n      B\n  > }\n    let b;"),
        ("class C {\n  constructor(public x = 1, readonly y?: string) {}\n}\n", ":2\nclass C {\n  constructor(public x = 1, readonly y?: string) {}\n                     ^^^^^\n}"),
        ("let a = 1, b = <any>x, c;\n", ":1\nlet a = 1, b = <any>x, c;\n               ^^^^^^"),
        ("module M.N { }\n", ":1\nmodule M.N { }\n^^^^^^^^"),
        ("export enum E { A }\n", ":1\nexport enum E { A }\n       ^^^^^^^^^^^^"),
        ("import x = require('y')\nlet z;\n", ":1\nimport x = require('y')\n^^^^^^^^^^^^^^^^^^^^^^^\nlet z;"),
        ("let a;\n\tenum\tE { A }\n", ":2\nlet a;\n    enum    E { A }\n    ^^^^^^^^^^^^^^^"),
        ("let a;\r\nenum E { A }\r\nlet b;\r\n", ":2\nlet a;\nenum E { A }\n^^^^^^^^^^^^\nlet b;\r"),
        ("const a = 1;\nlet x: = 2;\nlet z;\n", ":2\nconst a = 1;\nlet x: = 2;\n       ^\nlet z;"),
        ("let x: ", ":1\nlet x: "),
    ];
    for (src, want) in cases {
        assert_eq!(head(frame(src)), want, "{src:?}");
    }
    let full = frame("enum E { A }");
    assert!(full.ends_with(
        "\n\nSyntaxError [ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX]: TypeScript enum is not supported in strip-only mode"
    ));
}

/// Decorators before `export` (puppeteer's `@moveable export abstract class JSHandle`): Node's
/// strip text blanks that `export` (and keeps `abstract`); the engine runs the valid form.
#[test]
fn decorated_export() {
    assert_eq!(
        strip_types("@d export abstract class A {}").unwrap(),
        "@d        abstract class A {}"
    );
    assert_eq!(
        strip_types("@d export class A {}").unwrap(),
        "@d export class A {}"
    );
    assert_eq!(
        strip_types("@d export declare class A {}").unwrap(),
        "@d                          "
    );
    let e = strip_types("export @d abstract class A {}").unwrap_err();
    assert_eq!(e.code, Some(INVALID));
    let body = crate::parser::parse_module_ts("@d export abstract class A {}").unwrap();
    assert_eq!(body.len(), 1);
}
