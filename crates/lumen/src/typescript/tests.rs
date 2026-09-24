//! Unit tests: the type grammar and assignability, JSDoc extraction, tsconfig, and one sound
//! and one not-sound program per row of the soundness table (docs/typed-tier.md §4.2).
//! Offset and strip equivalence tests over fixture files live in `tests/`.

use super::types::{IndexSignature, TypeParam};
use super::*;

// ----- helpers -------------------------------------------------------------------------------

fn table_with(src: &str, lang: Lang, compiler: CompilerOptions) -> TypeTable {
    match analyze(src, &AnalyzeOptions { lang, compiler }) {
        Ok(a) => a.table,
        Err(d) => panic!("parse error {d}\n{src}"),
    }
}

fn ts(src: &str) -> TypeTable {
    table_with(src, Lang::Ts, CompilerOptions::default())
}

fn js(src: &str) -> TypeTable {
    table_with(src, Lang::Js, CompilerOptions::default())
}

fn func<'a>(t: &'a TypeTable, name: &str) -> &'a FnTypes {
    t.fns
        .iter()
        .map(|(_, f)| f)
        .find(|f| f.name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = t.fns.iter().map(|(_, f)| f.name.as_str()).collect();
            panic!("no function `{name}` in {names:?}")
        })
}

fn reason(t: &TypeTable, name: &str) -> String {
    let f = func(t, name);
    if f.sound {
        return "sound".into();
    }
    t.note_for(Subject::Fn(f.start))
        .map(|n| n.reason.clone())
        .unwrap_or_else(|| "not sound (no note)".into())
}

#[track_caller]
fn sound(t: &TypeTable, name: &str) {
    let r = reason(t, name);
    assert_eq!(r, "sound", "`{name}` should be sound");
}

#[track_caller]
fn unsound(t: &TypeTable, name: &str, why: &str) {
    let r = reason(t, name);
    assert!(r != "sound", "`{name}` should not be sound");
    assert!(
        r.contains(why),
        "`{name}`: reason `{r}` does not mention `{why}`"
    );
}

fn class<'a>(t: &'a TypeTable, name: &str) -> (u32, &'a ClassLayout) {
    t.classes
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == name)
        .map(|(i, c)| (i as u32, c))
        .unwrap_or_else(|| panic!("no class `{name}`"))
}

fn class_reason(t: &TypeTable, name: &str) -> String {
    let (i, c) = class(t, name);
    if c.sound {
        return "sound".into();
    }
    t.note_for(Subject::Class(i))
        .map(|n| n.reason.clone())
        .unwrap_or_else(|| "not sound (no note)".into())
}

/// The facts of kind `pred` whose source text is `text`.
fn sites_at<'a>(src: &str, f: &'a FnTypes, text: &str) -> Vec<&'a SiteFact> {
    f.sites
        .iter()
        .filter(|s| &src[s.start as usize..s.end as usize] == text)
        .map(|s| &s.fact)
        .collect()
}

fn has_check(src: &str, f: &FnTypes, text: &str, k: TKind) -> bool {
    sites_at(src, f, text)
        .iter()
        .any(|s| **s == SiteFact::Check(k.clone()))
}

// ----- type grammar and assignability (the original skeleton's tests, updated) ---------------

fn prop(name: &str, optional: bool, ty: Type) -> Property {
    Property {
        name: name.into(),
        optional,
        readonly: false,
        method: false,
        ty,
    }
}

#[test]
fn parses_composite_types() {
    assert_eq!(
        parse_type_expression("{ id: number; label?: string; tags: Array<string> } | null")
            .unwrap(),
        Type::Union(vec![
            Type::Object(ObjectType {
                props: vec![
                    prop("id", false, Type::Number),
                    prop("label", true, Type::String),
                    prop(
                        "tags",
                        false,
                        Type::Reference {
                            name: "Array".into(),
                            arguments: vec![Type::String]
                        }
                    ),
                ],
                ..ObjectType::default()
            }),
            Type::Null,
        ])
    );
}

#[test]
fn parses_function_tuple_and_array_types() {
    assert_eq!(
        parse_type_expression("(value: [number, string[]]) => boolean").unwrap(),
        Type::Function(Box::new(FnType {
            type_params: Vec::new(),
            this: None,
            params: vec![Param {
                name: "value".into(),
                ty: Type::Tuple(vec![Type::Number, Type::Array(Box::new(Type::String))]),
                optional: false,
                rest: false,
            }],
            ret: Type::Boolean,
            predicate: None,
            construct: false,
        }))
    );
}

#[test]
fn parses_advanced_types_as_opaque_or_structured() {
    for src in [
        "keyof T",
        "T extends string ? 1 : 2",
        "{ [K in keyof T]: T[K] }",
        "typeof x",
        "`a${string}`",
        "import('./m').T",
    ] {
        assert!(
            matches!(parse_type_expression(src).unwrap(), Type::Opaque(_)),
            "{src}"
        );
    }
    assert_eq!(
        parse_type_expression("readonly number[]").unwrap(),
        Type::ReadonlyArray(Box::new(Type::Number))
    );
    assert!(matches!(
        parse_type_expression("{ readonly [k: string]: number; m(): void }").unwrap(),
        Type::Object(ObjectType { ref index, ref props, .. })
            if index == &vec![IndexSignature { key: Type::String, value: Type::Number, readonly: true }]
            && props[0].method
    ));
    assert!(parse_type_expression("<T>(x: T) => T").is_ok());
    assert!(parse_type_expression("new (x: number) => Foo").is_ok());
    assert!(parse_type_expression("(x: unknown) => x is string").is_ok());
}

#[test]
fn reports_source_location() {
    let error = parse_type_expression("{\n value number\n}").unwrap_err();
    assert_eq!((error.code, error.span.line), (1005, 2));
}

#[test]
fn checks_structural_object_and_union_assignability() {
    let source = parse_type_expression("{ id: 1; name: 'lumen'; extra: boolean }").unwrap();
    let target = parse_type_expression("{ id: number; name?: string }").unwrap();
    assert!(is_assignable(&source, &target));
    assert!(is_assignable(
        &Type::StringLiteral("ok".into()),
        &parse_type_expression("number | string").unwrap()
    ));
    assert!(!is_assignable(&Type::Boolean, &target));
}

#[test]
fn checks_arrays_tuples_and_function_variance() {
    let t = |s: &str| parse_type_expression(s).unwrap();
    assert!(is_assignable(&t("[number, number]"), &t("number[]")));
    assert!(is_assignable(
        &t("(value: number | string) => string"),
        &t("(value: number) => string")
    ));
    assert!(!is_assignable(
        &t("(value: number) => string"),
        &t("(value: number | string) => string")
    ));
}

#[test]
fn assignability_fixes_the_skeletons_unsoundness() {
    let t = |s: &str| parse_type_expression(s).unwrap();
    // `any` is no longer assignable to everything (row 1).
    assert!(!is_assignable(&Type::Any, &Type::Number));
    assert!(is_assignable(&Type::Number, &Type::Any));
    assert!(is_assignable(&Type::Any, &Type::Unknown));
    // Mutable arrays are invariant; readonly arrays covariant (row 8).
    assert!(!is_assignable(&t("(number | string)[]"), &t("number[]")));
    assert!(!is_assignable(&t("number[]"), &t("(number | string)[]")));
    assert!(is_assignable(
        &t("number[]"),
        &t("readonly (number | string)[]")
    ));
    assert!(!is_assignable(&t("readonly number[]"), &t("number[]")));
    // Tuples into mutable arrays are invariant in the element too.
    assert!(!is_assignable(
        &t("[number, string]"),
        &t("(number | string)[]")
    ));
    assert!(is_assignable(
        &t("[number, string]"),
        &t("readonly (number | string)[]")
    ));
    // Literal types are widened: `1[]` and `number[]` are the same array type.
    assert!(is_assignable(&t("1[]"), &t("number[]")));
    // Arity must match (row 9, conservative).
    assert!(!is_assignable(&t("() => void"), &t("(x: number) => void")));
    // Optional parameters count as `T | undefined`.
    assert!(!is_assignable(
        &t("(x: number) => void"),
        &t("(x?: number) => void")
    ));
    assert!(is_assignable(
        &t("(x?: number) => void"),
        &t("(x: number) => void")
    ));
    // Intersections: a source intersection needs one member; a target needs all.
    assert!(is_assignable(
        &t("{a: number} & {b: string}"),
        &t("{a: number}")
    ));
    // Structural property types are covariant: object reads are checked at use (row 10) and
    // writes to class instances are guarded by typed shapes (§5.3), so this is safe.
    assert!(is_assignable(
        &t("{ a: number }"),
        &t("{ a: number | string }")
    ));
    // External values need a check before flowing anywhere narrower.
    assert!(!is_assignable(&Type::External, &Type::Number));
}

#[test]
fn union_normalizes() {
    assert_eq!(
        Type::union(vec![Type::Number, Type::NumberLiteral(1.0), Type::Never]),
        Type::Number
    );
    assert_eq!(
        Type::union(vec![
            Type::BooleanLiteral(true),
            Type::BooleanLiteral(false)
        ]),
        Type::Boolean
    );
    assert_eq!(Type::union(vec![]), Type::Never);
}

// ----- JSDoc ---------------------------------------------------------------------------------

#[test]
fn jsdoc_param_returns_type_this() {
    let d = jsdoc::parse_comment(
        "/**\n * Adds.\n * @param {number} a first\n * @param {string=} b\n * @param {number} [c=1] third\n * @param {...number} rest\n * @param {Object} opts.x skipped\n * @returns {boolean}\n * @this {Foo}\n */",
    );
    assert_eq!(d.params.len(), 4);
    assert_eq!(
        (d.params[0].name.as_str(), &d.params[0].ty),
        ("a", &Type::Number)
    );
    assert!(d.params[1].optional && d.params[1].ty == Type::String);
    assert!(d.params[2].optional && d.params[2].name == "c");
    assert!(d.params[3].rest && d.params[3].ty == Type::Number);
    assert_eq!(d.returns, Some(Type::Boolean));
    assert!(matches!(d.this, Some(Type::Reference { ref name, .. }) if name == "Foo"));
    assert!(d.errors.is_empty());

    let d = jsdoc::parse_comment("/** @type {Array<number>} */");
    assert_eq!(d.ty, Some(Type::Array(Box::new(Type::Number))));
    let d = jsdoc::parse_comment("/** @return {void} */");
    assert_eq!(d.returns, Some(Type::Void));
}

#[test]
fn jsdoc_closure_types() {
    let p = |s: &str| parse_jsdoc_type(s).unwrap().0;
    assert_eq!(p("*"), Type::Any);
    assert_eq!(p("?"), Type::Any);
    assert_eq!(p("Object"), Type::Any);
    assert_eq!(p("Function"), Type::Any);
    assert_eq!(p("Array"), Type::Any);
    assert_eq!(p("?number"), Type::union(vec![Type::Number, Type::Null]));
    assert_eq!(
        p("!Foo"),
        Type::Reference {
            name: "Foo".into(),
            arguments: vec![]
        }
    );
    assert_eq!(p("Array.<string>"), Type::Array(Box::new(Type::String)));
    assert!(
        matches!(p("function(number, string): boolean"), Type::Function(ref f) if f.params.len() == 2 && f.ret == Type::Boolean)
    );
    assert!(matches!(p("{a: number, b: string}"), Type::Object(ref o) if o.props.len() == 2));
    let (t, optional, rest) = parse_jsdoc_type("number=").unwrap();
    assert!(optional && !rest && t == Type::Number);
    let (_, optional, rest) = parse_jsdoc_type("...string").unwrap();
    assert!(!optional && rest);
    // `Object<string, number>` is a map type, not `any`.
    assert!(!matches!(p("Object<string, number>"), Type::Any));
}

#[test]
fn jsdoc_template_typedef_callback() {
    let d = jsdoc::parse_comment("/** @template T, U\n * @template {string} K\n */");
    let names: Vec<&str> = d.templates.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["T", "U", "K"]);
    assert_eq!(d.templates[2].constraint, Some(Type::String));

    let d = jsdoc::parse_comment(
        "/**\n * @typedef {Object} Point\n * @property {number} x\n * @property {number} [y]\n */",
    );
    assert_eq!(d.typedefs.len(), 1);
    let (name, ty) = &d.typedefs[0];
    assert_eq!(name, "Point");
    assert!(matches!(ty, Type::Object(o) if o.props.len() == 2 && o.props[1].optional));

    let d = jsdoc::parse_comment("/** @typedef {number | string} Id */");
    assert_eq!(
        d.typedefs[0],
        ("Id".into(), Type::union(vec![Type::Number, Type::String]))
    );

    let d = jsdoc::parse_comment(
        "/**\n * @callback Cmp\n * @param {number} a\n * @param {number} b\n * @returns {number}\n */",
    );
    assert!(
        matches!(&d.typedefs[0].1, Type::Function(f) if f.params.len() == 2 && f.ret == Type::Number)
    );
    assert!(
        d.params.is_empty(),
        "callback params are not the function's"
    );

    let d = jsdoc::parse_comment("/** @param {number x */");
    assert!(d.params.is_empty());
    let d = jsdoc::parse_comment("/** @param {number[} x */");
    assert_eq!(d.errors.len(), 1);
}

const CHECKED_JS: &str = r#"// @ts-check
/**
 * @typedef {Object} Point
 * @property {number} x
 * @property {number} y
 */

/**
 * @param {number} a
 * @param {number} b
 * @returns {number}
 */
function add(a, b) {
  return a + b;
}

/** @param {Point} p @returns {number} */
function px(p) {
  return p.x;
}

/**
 * @template T
 * @param {T} x
 * @returns {T}
 */
function id(x) {
  return x;
}

/** @type {(n: number) => number} */
const double = (n) => n * 2;

class Counter {
  /** @type {number} */
  count = 0;
  constructor() {
    /** @type {string} */
    this.label = "c";
  }
  /** @returns {number} */
  next() {
    this.count += 1;
    return this.count;
  }
}

/** @param {unknown} v @returns {number} */
function cast(v) {
  return /** @type {number} */ (v);
}

/** @param {unknown} v @returns {{a: number}} */
function badCast(v) {
  return /** @type {{a: number}} */ (v);
}

/** @param {*} v */
function star(v) {
  return v;
}

function untyped(a) {
  return a;
}
"#;

#[test]
fn jsdoc_functions_in_a_checked_js_file() {
    let t = js(CHECKED_JS);
    sound(&t, "add");
    assert_eq!(func(&t, "add").params, [TKind::Num, TKind::Num]);
    assert_eq!(func(&t, "add").ret, TKind::Num);
    sound(&t, "px");
    assert_eq!(func(&t, "px").params, [TKind::Object]);
    assert!(has_check(CHECKED_JS, func(&t, "px"), "p.x", TKind::Num));
    sound(&t, "id");
    assert_eq!(func(&t, "id").params, [TKind::Any]);
    sound(&t, "double");
    assert_eq!(func(&t, "double").params, [TKind::Num]);
    sound(&t, "Counter#next");
    let (_, c) = class(&t, "Counter");
    assert!(c.sound, "{}", class_reason(&t, "Counter"));
    assert_eq!(
        c.fields,
        [
            ("count".to_string(), TKind::Num, false),
            ("label".to_string(), TKind::Str, false)
        ]
    );
    sound(&t, "cast");
    assert!(has_check(CHECKED_JS, func(&t, "cast"), "v", TKind::Num));
    unsound(&t, "badCast", "unchecked cast");
    unsound(&t, "star", "any");
    unsound(&t, "untyped", "no type");
}

#[test]
fn jsdoc_is_hints_only_without_ts_check() {
    let src = CHECKED_JS.replacen("// @ts-check", "", 1);
    let t = js(&src);
    unsound(&t, "add", "hints only");
    // Stage-1 hints are still there.
    assert_eq!(func(&t, "add").params, [TKind::Num, TKind::Num]);
    let (_, c) = class(&t, "Counter");
    assert!(!c.sound);
    // checkJs turns checking on for the whole file.
    let t = table_with(
        &src,
        Lang::Js,
        CompilerOptions {
            check_js: true,
            ..CompilerOptions::default()
        },
    );
    sound(&t, "add");
    // `@ts-nocheck` wins over checkJs.
    let t = table_with(
        &format!("// @ts-nocheck\n{src}"),
        Lang::Js,
        CompilerOptions {
            check_js: true,
            ..CompilerOptions::default()
        },
    );
    unsound(&t, "add", "@ts-nocheck");
}

#[test]
fn jsdoc_attaches_to_the_nearest_preceding_comment() {
    let src = "// @ts-check\n/** @param {number} x @returns {number} */\n\n/** unrelated */\nfunction f(x) { return x; }\n/** @param {number} x @returns {number} */\nexport function g(x) { return x; }\n";
    let t = js(src);
    unsound(&t, "f", "no type");
    sound(&t, "g");
}

#[test]
fn jsdoc_comments_are_ignored_in_ts() {
    let t = ts("/** @param {number} x @returns {number} */\nfunction f(x) { return x; }");
    unsound(&t, "f", "no type");
}

// ----- the soundness table, row by row -------------------------------------------------------

#[test]
fn row01_any() {
    let src = r#"
function ok(x: number, s: unknown): number { return x + 1; }
function explicitAny(x: any): number { return 1; }
function implicitAny(x): number { return 1; }
function anyLocal(n: number): number { const v: any = n; return 1; }
function anyReturn(n: number): any { return n; }
function jsonParse(s: string): number { const v: number = JSON.parse(s); return v; }
function catchVar(): number { try { return 1; } catch (e) { return 2; } }
function catchAny(): number { try { return 1; } catch (e: any) { return 2; } }
function anyCast(n: number): number { return (n as any) + 1; }
function fnType(f: Function): number { return 1; }
"#;
    let t = ts(src);
    sound(&t, "ok");
    unsound(&t, "explicitAny", "`any`");
    unsound(&t, "implicitAny", "implicit any");
    unsound(&t, "anyLocal", "`any`");
    unsound(&t, "anyReturn", "`any`");
    // Library results are external values, checked where they enter a typed slot (§6.2).
    sound(&t, "jsonParse");
    assert!(has_check(
        src,
        func(&t, "jsonParse"),
        "JSON.parse(s)",
        TKind::Num
    ));
    // `catch (e)` is `unknown` under strict (useUnknownInCatchVariables).
    sound(&t, "catchVar");
    unsound(&t, "catchAny", "`any`");
    unsound(&t, "anyCast", "`any`");
    unsound(&t, "fnType", "`any`");
}

#[test]
fn row02_casts() {
    let src = r#"
class A { a: number = 1; }
interface I { a: number }
function toNum(v: unknown): number { return v as number; }
function toClass(v: unknown): number { return (v as A).a; }
function toArr(v: unknown): number[] { return <number[]>v; }
function upcast(n: number): number | string { return n as number | string; }
function asConst(): number { const x = 1 as const; return x; }
function toIface(v: unknown): I { return v as I; }
function doubleCast(v: string): number { return v as unknown as number; }
function toFn(v: unknown): () => void { return v as () => void; }
"#;
    let t = ts(src);
    sound(&t, "toNum");
    assert!(has_check(src, func(&t, "toNum"), "v", TKind::Num));
    sound(&t, "toClass");
    assert!(has_check(src, func(&t, "toClass"), "v", TKind::Class(0)));
    sound(&t, "toArr");
    assert!(has_check(src, func(&t, "toArr"), "v", TKind::NumArray));
    sound(&t, "upcast");
    assert!(
        func(&t, "upcast").sites.is_empty(),
        "an upcast needs no check"
    );
    sound(&t, "asConst");
    unsound(&t, "toIface", "unchecked cast");
    // `as unknown as T` is fine: the final cast is a checked one.
    sound(&t, "doubleCast");
    assert!(has_check(
        src,
        func(&t, "doubleCast"),
        "v as unknown",
        TKind::Num
    ));
    unsound(&t, "toFn", "unchecked cast");
}

#[test]
fn row03_non_null() {
    let src = r#"
class N { v: number = 0; }
function f(x: number | null): number { return x!; }
function g(n: N | undefined): number { return n!.v; }
function h(x: number | null): number { return x; }
"#;
    let t = ts(src);
    sound(&t, "f");
    assert!(has_check(src, func(&t, "f"), "x", TKind::Num));
    sound(&t, "g");
    assert!(has_check(src, func(&t, "g"), "n", TKind::Class(0)));
    unsound(&t, "h", "not assignable");
}

#[test]
fn row04_definite_assignment() {
    let src = r#"
function assigned(): number { let x: number; x = 1; return x; }
function bang(): number { let x!: number; return x; }
function noInit(): number { let x: number; return x; }
class C { x!: number; y: number = 0; }
function readBang(c: C): number { return c.x; }
function readInit(c: C): number { return c.y; }
"#;
    let t = ts(src);
    sound(&t, "assigned");
    unsound(&t, "bang", "not assignable");
    unsound(&t, "noInit", "not assignable");
    let (_, c) = class(&t, "C");
    assert!(c.sound);
    assert_eq!(c.fields[0].1, TKind::Tags(tag::NUM | tag::UNDEFINED));
    unsound(&t, "readBang", "not assignable");
    sound(&t, "readInit");
}

#[test]
fn row05_type_predicates() {
    let src = r#"
interface Shape { kind: string }
function isNum(x: unknown): x is number { return typeof x === "number"; }
function isShape(x: unknown): x is Shape { return x !== null; }
function usesNum(v: unknown): number { if (isNum(v)) { return v; } return 0; }
function usesShape(v: unknown): string { if (isShape(v)) { return v.kind; } return ""; }
function isFn(x: unknown): x is () => number { return typeof x === "function"; }
function usesFn(v: unknown): number { if (isFn(v)) { return v(); } return 0; }
function assertsNum(x: unknown): asserts x is number {}
function usesAsserts(v: unknown): number { assertsNum(v); return v; }
"#;
    let t = ts(src);
    sound(&t, "isNum");
    sound(&t, "usesNum");
    let f = func(&t, "usesNum");
    assert!(
        has_check(src, f, "v", TKind::Num),
        "a check at the narrowing call"
    );
    // A structural target is checked as "is an object"; its property reads are checked at use.
    sound(&t, "usesShape");
    let f = func(&t, "usesShape");
    assert!(has_check(src, f, "v", TKind::Object));
    assert!(has_check(src, f, "v.kind", TKind::Str));
    // A function type cannot be checked in O(1).
    unsound(&t, "usesFn", "row 5");
    // `asserts` predicates do not narrow in v1.
    unsound(&t, "usesAsserts", "not assignable");
}

#[test]
fn row06_declare() {
    let src = r#"
declare const limit: number;
declare const loose: any;
declare function ext(x: number): number;
declare class Foo { x: number }
class D { declare y: number; z: number = 0; }
function readDeclared(): number { return limit; }
function readAnyDeclared(): number { return loose; }
function callDeclared(): number { return ext(1); }
"#;
    let t = ts(src);
    sound(&t, "readDeclared");
    assert!(has_check(
        src,
        func(&t, "readDeclared"),
        "limit",
        TKind::Num
    ));
    unsound(&t, "readAnyDeclared", "`any`");
    sound(&t, "callDeclared");
    assert!(has_check(
        src,
        func(&t, "callDeclared"),
        "ext(1)",
        TKind::Num
    ));
    assert!(class_reason(&t, "D").contains("declare"));
    // Ambient functions and classes have no body and no table entry.
    assert!(t.fns.iter().all(|(_, f)| f.name != "ext"));
}

#[test]
fn row07_element_access() {
    let src = r#"
function elem(a: number[], i: number): number { return a[i]; }
function strElem(a: string[], i: number): string { return a[i]; }
function record(r: { [k: string]: number }, k: string): number { return r[k]; }
function recordDot(r: Record<string, number>): number { return r.foo; }
function handled(a: number[], i: number): number { return a[i] ?? 0; }
function unhandled(a: number[], i: number): number { return a[i]; }
"#;
    let t = ts(src);
    sound(&t, "elem");
    assert_eq!(
        sites_at(src, func(&t, "elem"), "a[i]"),
        [&SiteFact::Elem(TKind::Num)]
    );
    sound(&t, "strElem");
    assert_eq!(
        sites_at(src, func(&t, "strElem"), "a[i]"),
        [&SiteFact::Elem(TKind::Str)]
    );
    sound(&t, "record");
    assert!(has_check(src, func(&t, "record"), "r[k]", TKind::Num));
    sound(&t, "recordDot");
    assert!(has_check(src, func(&t, "recordDot"), "r.foo", TKind::Num));
    let t = table_with(
        src,
        Lang::Ts,
        CompilerOptions {
            no_unchecked_indexed_access: true,
            ..CompilerOptions::default()
        },
    );
    sound(&t, "handled");
    unsound(&t, "unhandled", "not assignable");
}

#[test]
fn row08_array_variance() {
    let src = r#"
class Animal { name: string = ""; }
class Dog extends Animal { bark: number = 0; }
function covariant(d: Dog[]): Animal[] { return d; }
function viaLocal(d: Dog[]): number { const a: Animal[] = d; return a.length; }
function readonlyOk(d: Dog[]): readonly Animal[] { return d; }
function sameType(d: Dog[]): Dog[] { return d; }
function pushReadonly(a: readonly number[]): number { return a.push(1); }
function writeReadonly(a: readonly number[]): number { a[0] = 1; return 0; }
function pushOk(a: number[]): number { return a.push(1); }
function pushBad(a: number[]): number { a.push("x"); return 0; }
"#;
    let t = ts(src);
    unsound(&t, "covariant", "invariant");
    unsound(&t, "viaLocal", "invariant");
    sound(&t, "readonlyOk");
    sound(&t, "sameType");
    unsound(&t, "pushReadonly", "readonly");
    unsound(&t, "writeReadonly", "readonly");
    sound(&t, "pushOk");
    unsound(&t, "pushBad", "not assignable");
}

#[test]
fn row09_function_variance() {
    let src = r#"
type Handler = (x: number | string) => void;
type NumHandler = (x: number) => void;
interface Box { m(x: number | string): void }
function narrowParam(h: (x: number) => void): Handler { return h; }
function wideParam(h: Handler): NumHandler { return h; }
function fewerParams(h: () => void): NumHandler { return h; }
function optionalVsRequired(h: (x: number) => void): (x?: number) => void { return h; }
function methodBivariance(f: (x: number) => void): Box { return { m: f }; }
"#;
    let t = ts(src);
    unsound(&t, "narrowParam", "not assignable");
    sound(&t, "wideParam");
    // TS accepts this; the subset requires matching arity (conservative).
    unsound(&t, "fewerParams", "not assignable");
    unsound(&t, "optionalVsRequired", "not assignable");
    unsound(&t, "methodBivariance", "not assignable");
}

#[test]
fn row10_structural_types() {
    let src = r#"
interface P { x: number; y?: number }
function readX(p: P): number { return p.x; }
function readY(p: P): number { return p.y; }
function readYOk(p: P): number { return p.y ?? 0; }
function missing(p: P): number { return p.z; }
"#;
    let t = ts(src);
    sound(&t, "readX");
    assert_eq!(func(&t, "readX").params, [TKind::Object]);
    assert!(has_check(src, func(&t, "readX"), "p.x", TKind::Num));
    unsound(&t, "readY", "not assignable");
    sound(&t, "readYOk");
    unsound(&t, "missing", "no property");
}

#[test]
fn row11_narrowing_and_closures() {
    let src = r#"
function plain(v: number | string): number {
  if (typeof v === "number") { return v; }
  return v.length;
}
function closureWrites(v: number | string): number {
  let x: number | string = v;
  const set = () => { x = "s"; };
  if (typeof x === "number") { set(); return x; }
  return 0;
}
function loopInvalidates(v: number | null): number {
  let x: number | null = v;
  if (x !== null) {
    while (true) { x = null; break; }
    return x;
  }
  return 0;
}
function afterReturn(v: string | undefined): string {
  if (v === undefined) return "";
  return v;
}
function andNarrow(a: number | null, b: number | null): number {
  if (a !== null && b !== null) return a + b;
  return 0;
}
function orNarrow(a: number | null): number {
  if (a === null || a < 0) return 0;
  return a;
}
function truthy(s: string | null): string { return s ? s : ""; }
function looseNull(v: number | null | undefined): number { if (v != null) return v; return 0; }
"#;
    let t = ts(src);
    sound(&t, "plain");
    assert!(has_check(src, func(&t, "plain"), "v", TKind::Num));
    unsound(&t, "closureWrites", "not assignable");
    unsound(&t, "loopInvalidates", "not assignable");
    sound(&t, "afterReturn");
    sound(&t, "andNarrow");
    sound(&t, "orNarrow");
    sound(&t, "truthy");
    sound(&t, "looseNull");
}

#[test]
fn row12_overrides() {
    let src = r#"
class Base { x: number | undefined = 0; readonly r: number | string = 0; m(x: number | string): void {} n(x: number): number { return x; } }
class FieldNarrowed extends Base { x: number = 0; }
class ReadonlyNarrowed extends Base { readonly r: number = 0; }
class MethodNarrowed extends Base { m(x: number): void {} }
class MethodWidened extends Base { n(x: number | string): number { return 0; } }
"#;
    let t = ts(src);
    assert!(class(&t, "Base").1.sound);
    assert!(class_reason(&t, "FieldNarrowed").contains("invariant"));
    assert!(
        class(&t, "ReadonlyNarrowed").1.sound,
        "{}",
        class_reason(&t, "ReadonlyNarrowed")
    );
    assert!(class_reason(&t, "MethodNarrowed").contains("row 12"));
    assert!(
        class(&t, "MethodWidened").1.sound,
        "{}",
        class_reason(&t, "MethodWidened")
    );
}

#[test]
fn row13_this() {
    let src = r#"
class K { v: number = 1; get(): number { return this.v; } arrow(): number { const f = (): number => this.v; return f(); } }
function annotated(this: K): number { return this.v; }
function implicitThis(): number { return this.v; }
const obj = { v: 1, m(): number { return this.v; } };
"#;
    let t = ts(src);
    sound(&t, "K#get");
    assert_eq!(func(&t, "K#get").this, TKind::Class(0));
    sound(&t, "K#arrow");
    sound(&t, "annotated");
    assert_eq!(func(&t, "annotated").this, TKind::Class(0));
    unsound(&t, "implicitThis", "row 13");
    // Object-literal `this` is dynamic: the read is checked at the return.
    sound(&t, "m");
}

#[test]
fn row14_definite_assignment_in_constructors() {
    let src = r#"
class Ok { a: number; b: string; constructor(a: number) { this.a = a; this.b = "x"; } }
class Escapes { a: number; constructor() { this.init(); this.a = 1; } init(): void {} }
class Branchy { a: number; constructor(f: boolean) { if (f) { this.a = 1; } } }
class BothBranches { a: number; constructor(f: boolean) { if (f) { this.a = 1; } else { this.a = 2; } } }
class ReadBefore { a: number; b: number; constructor() { this.b = this.a; this.a = 1; } }
class Initializer { a: number = 1; b: number = this.a + 1; }
class Parent { p: number; constructor() { this.hook(); this.p = 1; } hook(): void {} }
class Child extends Parent { c: number = 1; }
"#;
    let t = ts(src);
    let n = TKind::Num;
    let nu = TKind::Tags(tag::NUM | tag::UNDEFINED);
    assert_eq!(class(&t, "Ok").1.fields[0].1, n);
    assert_eq!(class(&t, "Escapes").1.fields[0].1, nu);
    assert_eq!(class(&t, "Branchy").1.fields[0].1, nu);
    assert_eq!(class(&t, "BothBranches").1.fields[0].1, n);
    assert_eq!(class(&t, "ReadBefore").1.fields[0].1, nu);
    assert_eq!(class(&t, "ReadBefore").1.fields[1].1, nu);
    assert_eq!(class(&t, "Initializer").1.fields[1].1, n);
    assert_eq!(
        class(&t, "Child").1.fields[0].1,
        nu,
        "the parent constructor lets `this` escape"
    );
}

#[test]
fn row15_enums() {
    let src = r#"
enum E { A, B }
function f(e: E): number { return e; }
function g(): number { return E.A; }
"#;
    let t = ts(src);
    sound(&t, "f");
    assert_eq!(func(&t, "f").params, [TKind::Num]);
    sound(&t, "g");
    // The strip rejects enums, so such a file never runs typed.
    assert!(strip_types(src).is_err());
}

#[test]
fn row16_overloads() {
    let src = r#"
function f(x: number): number;
function f(x: string): string;
function f(x: number | string): number | string { return x; }
function g(): number { return f(1) as number; }
"#;
    let t = ts(src);
    assert_eq!(t.fns.iter().filter(|(_, f)| f.name == "f").count(), 1);
    sound(&t, "f");
    assert_eq!(func(&t, "f").params, [TKind::Tags(tag::NUM | tag::STR)]);
    sound(&t, "g");
}

#[test]
fn row17_suppressions() {
    let src = r#"
function inside(): number {
  // @ts-ignore
  return "x";
}
// @ts-expect-error
function nextLine(x): number { return 1; }
function clean(): number { return 1; }
"#;
    let t = ts(src);
    unsound(&t, "inside", "row 17");
    unsound(&t, "nextLine", "row 17");
    sound(&t, "clean");
    let t = ts(&format!("// @ts-nocheck\n{src}"));
    unsound(&t, "clean", "@ts-nocheck");
}

#[test]
fn row18_strict_null_checks() {
    let src = "function f(x: number): number { return x; }";
    sound(&ts(src), "f");
    let t = table_with(
        src,
        Lang::Ts,
        CompilerOptions {
            strict_null_checks: false,
            ..CompilerOptions::default()
        },
    );
    unsound(&t, "f", "strictNullChecks");
    assert_eq!(func(&t, "f").params, [TKind::Num], "hints survive");
}

#[test]
fn row19_instanceof_and_structural_narrowing() {
    let src = r#"
class A { a: number = 1; }
interface S { s: number }
function viaInstanceof(v: unknown): number { if (v instanceof A) { return v.a; } return 0; }
function viaIn(v: S | A): number { if ("s" in v) { return v.s; } return 0; }
"#;
    let t = ts(src);
    sound(&t, "viaInstanceof");
    let f = func(&t, "viaInstanceof");
    assert!(sites_at(src, f, "a").contains(&&SiteFact::Field { class: 0, index: 0 }));
    unsound(&t, "viaIn", "union");
}

#[test]
fn row20_generics() {
    let src = r#"
function id<T>(x: T): T { return x; }
function useId(): number { return id(1); }
function first<T>(xs: T[]): T | undefined { return xs[0]; }
function bad<T>(x: T): number { return x; }
"#;
    let t = ts(src);
    sound(&t, "id");
    assert_eq!(func(&t, "id").params, [TKind::Any]);
    sound(&t, "useId");
    assert!(has_check(src, func(&t, "useId"), "id(1)", TKind::Num));
    sound(&t, "first");
    // A `T` value flowing into `number` is checked.
    sound(&t, "bad");
    assert!(has_check(src, func(&t, "bad"), "x", TKind::Num));
}

#[test]
fn row21_eval_with_arguments() {
    let src = r#"
function usesEval(s: string): number { eval(s); return 1; }
function usesArguments(): number { return arguments.length; }
function ok(): number { return 1; }
"#;
    let t = ts(src);
    unsound(&t, "usesEval", "eval");
    unsound(&t, "usesArguments", "arguments");
    sound(&t, "ok");
    // `eval` anywhere in the module makes bindings unstable: no direct calls.
    let src2 = "function a(): number { return 1; }\nfunction b(): number { return a(); }\n";
    let t = ts(src2);
    assert!(func(&t, "b")
        .sites
        .iter()
        .any(|s| matches!(s.fact, SiteFact::Callee(_))));
    let t = ts(&format!(
        "{src2}function c(s: string): void {{ eval(s); }}\n"
    ));
    assert!(!func(&t, "b")
        .sites
        .iter()
        .any(|s| matches!(s.fact, SiteFact::Callee(_))));
    let t = js("function w(o) { with (o) { x; } }");
    assert!(!func(&t, "w").sound);
}

#[test]
fn row22_method_reassignment() {
    let src = r#"
class M { m(): number { return 1; } reassign(): void { this.m = () => 2; } call(): number { return this.m(); } }
"#;
    let t = ts(src);
    unsound(&t, "M#reassign", "row 22");
    sound(&t, "M#call");
}

#[test]
fn row23_coercions() {
    let src = r#"
class O { v: number = 1; }
function concat(o: O): string { return o + ""; }
function bitor(o: O): number { return (o as unknown as number) | 0; }
function objOr(o: O): number { return o | 0; }
"#;
    let t = ts(src);
    sound(&t, "concat");
    sound(&t, "bitor");
    sound(&t, "objOr");
    assert!(has_check(src, func(&t, "objOr"), "o | 0", TKind::Num));
}

#[test]
fn row24_async_and_generators() {
    let src = r#"
async function a(): Promise<number> { return 1; }
function* g(): Generator<number> { yield 1; }
function s(): number { return 1; }
"#;
    let t = ts(src);
    unsound(&t, "a", "async");
    unsound(&t, "g", "generator");
    sound(&t, "s");
}

// ----- other subset rules ----------------------------------------------------------------

#[test]
fn not_in_subset() {
    let src = r#"
function destructure({ a }: { a: number }): number { return a; }
function missingReturn(x: number): number { if (x > 0) { return 1; } }
function spread(a: number[]): number { return Math.max(...a); }
function ok(x: number): number { if (x > 0) { return 1; } else { return 2; } }
function throwsAtEnd(x: number): number { if (x > 0) { return 1; } throw new Error("x"); }
function inferred(x: number) { return x * 2; }
function callsInferred(): number { return inferred(1); }
"#;
    let t = ts(src);
    unsound(&t, "destructure", "destructur");
    unsound(&t, "missingReturn", "without returning");
    unsound(&t, "spread", "spread");
    sound(&t, "ok");
    sound(&t, "throwsAtEnd");
    sound(&t, "inferred");
    assert_eq!(func(&t, "inferred").ret, TKind::Num);
    sound(&t, "callsInferred");
    // An unannotated return is not a trusted signature for callers.
    assert!(has_check(
        src,
        func(&t, "callsInferred"),
        "inferred(1)",
        TKind::Num
    ));
}

#[test]
fn trust_of_module_and_captured_bindings() {
    let src = r#"
const K = 10;
let counter: number = 0;
function readConst(): number { return K; }
function readLet(): number { return counter; }
function outer(n: number): () => number {
  const inner = (): number => n;
  return inner;
}
"#;
    let t = ts(src);
    sound(&t, "readConst");
    assert!(func(&t, "readConst").sites.is_empty());
    sound(&t, "readLet");
    assert!(has_check(src, func(&t, "readLet"), "counter", TKind::Num));
    sound(&t, "inner");
    let f = func(&t, "inner");
    assert!(f
        .sites
        .iter()
        .any(|s| s.fact == SiteFact::Check(TKind::Num)));
}

#[test]
fn intrinsics() {
    let src = r#"
function m(x: number): number { return Math.sqrt(x) + Math.PI + Math.max(x, 1); }
function n(x: number): boolean { return Number.isInteger(x); }
function s(v: string): number { return v.length + v.charCodeAt(0); }
function a(v: number[]): number { v.push(1); return v.length; }
function p(v: number[]): number { return v.pop() ?? 0; }
function shadowed(Math: { sqrt(x: number): string }): number { return Math.sqrt(1); }
function lib(v: string): string { return v.toUpperCase(); }
"#;
    let t = ts(src);
    for name in ["m", "n", "s", "a", "p"] {
        sound(&t, name);
        assert!(
            func(&t, name)
                .sites
                .iter()
                .all(|s| !matches!(s.fact, SiteFact::Check(_))),
            "{name}"
        );
    }
    unsound(&t, "shadowed", "not assignable");
    sound(&t, "lib");
    assert!(has_check(
        src,
        func(&t, "lib"),
        "v.toUpperCase()",
        TKind::Str
    ));
}

#[test]
fn direct_calls_and_callee_facts() {
    let src = r#"
class V { x: number = 0; len(): number { return this.x; } }
class W extends V { len(): number { return 1; } }
class Leaf { y: number = 0; get(): number { return this.y; } }
function add(a: number, b: number): number { return a + b; }
function bad(a: any): number { return 1; }
function caller(v: V, l: Leaf): number { return add(1, 2) + bad(1) + v.len() + l.get(); }
let reassigned = function (): number { return 1; };
function callsReassigned(): number { reassigned = () => 2; return reassigned(); }
"#;
    let t = ts(src);
    sound(&t, "caller");
    let f = func(&t, "caller");
    let add_key = func(&t, "add").start;
    let get_key = func(&t, "Leaf#get").start;
    assert_eq!(sites_at(src, f, "add(1, 2)"), [&SiteFact::Callee(add_key)]);
    // A call to a non-sound callee is checked instead.
    assert!(has_check(src, f, "bad(1)", TKind::Num));
    // `len` is overridden in a subclass: no direct call (CHA), result checked.
    assert!(has_check(src, f, "v.len()", TKind::Num));
    assert_eq!(sites_at(src, f, "l.get()"), [&SiteFact::Callee(get_key)]);
    sound(&t, "callsReassigned");
    assert!(!func(&t, "callsReassigned")
        .sites
        .iter()
        .any(|s| matches!(s.fact, SiteFact::Callee(_))));
}

#[test]
fn class_layouts() {
    let src = r#"
class P { x: number = 0; #secret: string = ""; static count: number = 0; readonly id: string = ""; }
class Q extends P { z: boolean = false; }
class Untyped { a = compute(); }
class Computed { ["k"]: number = 1; }
class ParamProp { constructor(public a: number) {} }
class External extends Array<number> { q: number = 0; }
class OnUnsound extends Untyped { b: number = 0; }
function compute(): number { return 1; }
function useQ(q: Q): number { return q.x; }
"#;
    let t = ts(src);
    let (pi, p) = class(&t, "P");
    assert!(p.sound);
    assert_eq!(
        p.fields,
        [
            ("x".to_string(), TKind::Num, false),
            ("#secret".to_string(), TKind::Str, false),
            ("id".to_string(), TKind::Str, true)
        ]
    );
    let (qi, q) = class(&t, "Q");
    assert_eq!(q.parent, Some(pi));
    assert_eq!(q.full_fields(&t.classes).len(), 4);
    assert!(class_reason(&t, "Untyped").contains("no type"));
    assert!(class_reason(&t, "Computed").contains("computed"));
    assert!(class_reason(&t, "ParamProp").contains("parameter property"));
    assert!(class_reason(&t, "External").contains("not a class of this file"));
    assert!(!class(&t, "OnUnsound").1.sound);
    sound(&t, "useQ");
    // Inherited field: the slot index is in the full layout.
    let f = func(&t, "useQ");
    assert_eq!(f.params, [TKind::Class(qi)]);
    assert!(sites_at(src, f, "x").contains(&&SiteFact::Field {
        class: qi,
        index: 0
    }));
    // Class instances with unsound layouts project to Object.
    let t2 = ts("class U { a; }\nfunction f(u: U): number { return 1; }");
    assert_eq!(func(&t2, "f").params, [TKind::Object]);
    let t3 = table_with(
        "class C { a: number = 1; }",
        Lang::Ts,
        CompilerOptions {
            use_define_for_class_fields: false,
            ..CompilerOptions::default()
        },
    );
    assert!(class_reason(&t3, "C").contains("useDefineForClassFields"));
}

#[test]
fn projections() {
    let src = r#"
class C { v: number = 0; }
function f(
  a: number | undefined,
  b: C | null,
  c: C | undefined,
  d: string[],
  e: [number, number],
  g: (x: number) => string,
  h: "a" | "b",
  i: symbol,
  j: bigint,
  k: unknown,
  l: object,
  m?: number,
  ...r: number[]
): void {}
"#;
    let t = ts(src);
    let f = func(&t, "f");
    let fn_sig = Signature {
        params: vec![TKind::Num],
        ret: TKind::Str,
    };
    let fn_kind = TKind::Func(t.sigs.iter().position(|s| *s == fn_sig).expect("sig") as u32);
    assert_eq!(
        f.params,
        [
            TKind::Tags(tag::NUM | tag::UNDEFINED),
            TKind::NullableClass(0),
            TKind::ClassOr(0, tag::UNDEFINED),
            TKind::Array(Box::new(TKind::Str)),
            TKind::NumArray,
            fn_kind,
            TKind::Str,
            TKind::Sym,
            TKind::BigInt,
            TKind::Any,
            TKind::Object,
            TKind::Tags(tag::NUM | tag::UNDEFINED),
            TKind::NumArray,
        ]
    );
}

#[test]
fn engine_offsets() {
    // Keys are FnSource::Range.start: `async`/`function` for declarations, the member start
    // (after `static`, including get/set/async/`*`) for methods, the parameter list for
    // arrows, and the `class` keyword for constructors.
    let src = "export async function a(): Promise<void> {}\nclass K {\n  static s(): void {}\n  get g(): number { return 1; }\n  async *gen(): AsyncGenerator<number> {}\n  constructor() {}\n}\nconst f = async (x: number): Promise<number> => x;\nconst h = (y: number): number => y;\nexport default function (): void {}\n";
    let t = ts(src);
    let at = |name: &str| &src[func(&t, name).start as usize..];
    assert!(at("a").starts_with("async function a"));
    assert!(at("K.s").starts_with("s(): void"));
    assert!(at("get K#g").starts_with("get g()"));
    assert!(at("K#gen").starts_with("async *gen"));
    assert!(at("K").starts_with("class K"));
    assert_eq!(
        func(&t, "K").end as usize,
        src.find("}\nconst f").unwrap() + 1
    );
    assert!(at("f").starts_with("async (x"));
    assert!(at("h").starts_with("(y: number)"));
    assert!(at("<function>").starts_with("function (): void"));
    // Locals are keyed by the binding identifier.
    let src = "function f(a: number): number { const b = a; return b; }";
    let t = ts(src);
    let offs: Vec<&str> = func(&t, "f")
        .locals
        .iter()
        .map(|(o, _)| &src[*o as usize..*o as usize + 1])
        .collect();
    assert_eq!(offs, ["a", "b"]);
}

#[test]
fn reports() {
    let src = "class P { x: number = 0; }\nfunction ok(p: P): number { return p.x; }\nfunction bad(x: any): number { return x; }\n";
    let t = ts(src);
    let text = report_text(&t, src, "a.ts");
    assert!(text.contains("a.ts:1:1"), "{text}");
    assert!(text.contains("layout: 1 fields (x: number)"), "{text}");
    assert!(text.contains("ok(p: class#0): number"), "{text}");
    assert!(
        text.contains("not sound: parameter `x` is `any` (row 1) (a.ts:3:14)"),
        "{text}"
    );
    assert!(text.ends_with("a.ts: 1/2 functions sound\n"), "{text}");
    let json = report_json(&t, src, "a.ts");
    assert!(
        json.starts_with("{\"file\":\"a.ts\",\"sound\":1,\"total\":2,"),
        "{json}"
    );
    assert!(
        json.contains("\"kind\":\"field\",\"class\":0,\"index\":0"),
        "{json}"
    );
    assert!(
        json.contains("\"reason\":\"parameter `x` is `any` (row 1)\""),
        "{json}"
    );
    let facts = dump_facts(&t, src);
    assert!(facts.contains("site @"), "{facts}");
}

#[test]
fn line_index_and_fmt_num() {
    let li = LineIndex::new("ab\ncé\nd");
    assert_eq!(li.line_col(0), (1, 1));
    assert_eq!(li.line_col(3), (2, 1));
    assert_eq!(li.line_col(6), (2, 3));
    assert_eq!(li.line_col(7), (3, 1));
    assert_eq!(fmt_num(1.0), "1");
    assert_eq!(fmt_num(1.5), "1.5");
    assert_eq!(fmt_num(f64::NAN), "NaN");
    let _ = TypeParam {
        name: "T".into(),
        constraint: None,
        default: None,
    };
}

#[test]
fn parser_survives_odd_inputs() {
    for src in [
        "",
        "let x = a < b > c;",
        "f<T>(x);",
        "const re = /[/]/g; const d = a / b / c;",
        "label: for (;;) { break label; }",
        "x = y ? (z) : w;",
        "const t = `a${`b${c}`}`;",
        "class A { static { let x = 1; } #p = 1; static #q() {} accessor z = 1; }",
        "let v = <T,>(x: T) => x;",
        "type A = { [K in keyof B as `x${K}`]?: B[K] };",
        "abstract class B { abstract m(): void; protected x?: number; }",
        "import type { X } from './x'; export type { Y } from './y'; import z = require('z');",
        "declare module 'm' { export const a: number; }",
        "namespace N { export const a = 1; }",
        "let a = b satisfies C; let d = e!;",
        "for await (const x of y) {}",
        "a?.b?.[c]?.(d);",
        "x ??= 1; y ||= 2; z &&= 3; w **= 2;",
        "function f(this: Window, ...rest: number[]) {}",
        "if (a) function g() {}",
    ] {
        let _ = analyze(src, &AnalyzeOptions::default());
        let _ = analyze(
            src,
            &AnalyzeOptions {
                lang: Lang::Js,
                compiler: CompilerOptions::default(),
            },
        );
    }
    for src in [
        "class A extends mixin(B) {}",
        "class A extends ns.Base<number> implements I {}",
        "class A extends (cond ? B : C) {}",
    ] {
        analyze(src, &AnalyzeOptions::default()).unwrap_or_else(|d| panic!("{src}: {d}"));
    }
    assert!(analyze("function (", &AnalyzeOptions::default()).is_err());
    assert!(analyze("let x = ;", &AnalyzeOptions::default()).is_err());
}
