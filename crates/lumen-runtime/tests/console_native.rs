//! `console.log` and friends format and write natively (no util/stream/tty), byte-for-byte as
//! Node's `util.inspect` / `util.format` do, and stay ordered against `process.stdout.write`.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{ConsoleOut, RealmExit, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `source` as a CommonJS main module; returns (stdout, stderr).
fn run(source: &str) -> (String, String) {
    let mut runtime = Runtime::new();
    let (out, err) = (Captured::default(), Captured::default());
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    assert!(matches!(
        runtime.run_embedded_source("console_native.js", source),
        RealmExit::Exited(0)
    ));
    let text = |c: Captured| String::from_utf8(c.0.borrow().clone()).unwrap();
    (text(out), text(err))
}

/// Expected output recorded from Node 26 (`node -e <source>`).
#[test]
fn console_output_matches_node() {
    let cases: &[(&str, &str, &str)] = &[
        ("console.log(1, -0, 1.5, 1e21, NaN, 10n, true, null, undefined)", "1 -0 1.5 1e+21 NaN 10n true null undefined\n", ""),
        ("console.log('str', \"it's\", ['it\\'s', 'say \"hi\"', 'both \\' and \"', 'a\\nb', '\\x01\\x7f'])", "str it's [ \"it's\", 'say \"hi\"', `both ' and \"`, 'a\\nb', '\\x01\\x7F' ]\n", ""),
        ("console.log(Symbol('s'), [Symbol()], {s: Symbol.iterator})", "Symbol(s) [ Symbol() ] { s: Symbol(Symbol.iterator) }\n", ""),
        ("console.log({}, [], {a:1}, [1,2,3], [[]], {a:{}})", "{} [] { a: 1 } [ 1, 2, 3 ] [ [] ] { a: {} }\n", ""),
        ("console.log({a:{b:{c:{d:1}}}}, {a:[[[1]]]})", "{ a: { b: { c: [Object] } } } { a: [ [ [Array] ] ] }\n", ""),
        ("console.log({'a-b':1, 2:3, valid_id:4, '__proto__x':5})", "{ '2': 3, 'a-b': 1, valid_id: 4, __proto__x: 5 }\n", ""),
        ("console.log([undefined, null, 'a'], {u: undefined, n: null})", "[ undefined, null, 'a' ] { u: undefined, n: null }\n", ""),
        ("console.log(Array.from({length:7},(_, i)=>i))", "[\n  0, 1, 2, 3,\n  4, 5, 6\n]\n", ""),
        ("console.log(Array.from({length:30},(_, i)=>i*3))", "[\n   0,  3,  6,  9, 12, 15, 18, 21, 24,\n  27, 30, 33, 36, 39, 42, 45, 48, 51,\n  54, 57, 60, 63, 66, 69, 72, 75, 78,\n  81, 84, 87\n]\n", ""),
        ("console.log(Array.from({length:101},(_, i)=>i))", "[\n   0,  1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11,\n  12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,\n  24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,\n  36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,\n  48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59,\n  60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71,\n  72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83,\n  84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95,\n  96, 97, 98, 99,\n  ... 1 more item\n]\n", ""),
        ("console.log(Array.from({length:150},(_, i)=>'item'+i))", "[\n  'item0',  'item1',  'item2',  'item3',  'item4',  'item5',\n  'item6',  'item7',  'item8',  'item9',  'item10', 'item11',\n  'item12', 'item13', 'item14', 'item15', 'item16', 'item17',\n  'item18', 'item19', 'item20', 'item21', 'item22', 'item23',\n  'item24', 'item25', 'item26', 'item27', 'item28', 'item29',\n  'item30', 'item31', 'item32', 'item33', 'item34', 'item35',\n  'item36', 'item37', 'item38', 'item39', 'item40', 'item41',\n  'item42', 'item43', 'item44', 'item45', 'item46', 'item47',\n  'item48', 'item49', 'item50', 'item51', 'item52', 'item53',\n  'item54', 'item55', 'item56', 'item57', 'item58', 'item59',\n  'item60', 'item61', 'item62', 'item63', 'item64', 'item65',\n  'item66', 'item67', 'item68', 'item69', 'item70', 'item71',\n  'item72', 'item73', 'item74', 'item75', 'item76', 'item77',\n  'item78', 'item79', 'item80', 'item81', 'item82', 'item83',\n  'item84', 'item85', 'item86', 'item87', 'item88', 'item89',\n  'item90', 'item91', 'item92', 'item93', 'item94', 'item95',\n  'item96', 'item97', 'item98', 'item99',\n  ... 50 more items\n]\n", ""),
        ("console.log(Array.from({length:27},(_, i)=>'x'.repeat(i%9)))", "[\n  '',       'x',       'xx',\n  'xxx',    'xxxx',    'xxxxx',\n  'xxxxxx', 'xxxxxxx', 'xxxxxxxx',\n  '',       'x',       'xx',\n  'xxx',    'xxxx',    'xxxxx',\n  'xxxxxx', 'xxxxxxx', 'xxxxxxxx',\n  '',       'x',       'xx',\n  'xxx',    'xxxx',    'xxxxx',\n  'xxxxxx', 'xxxxxxx', 'xxxxxxxx'\n]\n", ""),
        ("console.log(Array.from({length:7},(_, i)=>({id:i})))", "[\n  { id: 0 },\n  { id: 1 },\n  { id: 2 },\n  { id: 3 },\n  { id: 4 },\n  { id: 5 },\n  { id: 6 }\n]\n", ""),
        ("console.log(Array.from({length:8},(_, i)=>10n**BigInt(i)))", "[\n        1n,       10n,\n      100n,     1000n,\n    10000n,   100000n,\n  1000000n, 10000000n\n]\n", ""),
        ("console.log({a:'x'.repeat(100)}, ['y'.repeat(80)])", "{\n  a: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\n} [\n  'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy'\n]\n", ""),
        ("console.log({s:'line1\\nline2\\nline3 is a long line that goes beyond the limit of the break length for sure yes'})", "{\n  s: 'line1\\n' +\n    'line2\\n' +\n    'line3 is a long line that goes beyond the limit of the break length for sure yes'\n}\n", ""),
        ("class Foo { constructor(){ this.a=1; this.b='x'; } } class Bar extends Foo { constructor(){ super(); this.c=[1,2]; } } class Empty {} console.log(new Foo(), new Bar(), new Empty(), [new Foo()], {f: new Empty()})", "Foo { a: 1, b: 'x' } Bar { a: 1, b: 'x', c: [ 1, 2 ] } Empty {} [ Foo { a: 1, b: 'x' } ] { f: Empty {} }\n", ""),
        ("const n = Object.create(null); n.x = 1; console.log(n, Object.create(null), {n})", "[Object: null prototype] { x: 1 } [Object: null prototype] {} { n: [Object: null prototype] { x: 1 } }\n", ""),
        ("const c = {a:1}; c.self = c; console.log(c, {x:[c]})", "<ref *1> { a: 1, self: [Circular *1] } { x: [ <ref *1> { a: 1, self: [Circular *1] } ] }\n", ""),
        ("console.log({get x(){return 1}}, {set x(v){}}, {get x(){return 1}, set x(v){}})", "{ x: [Getter] } { x: [Setter] } { x: [Getter/Setter] }\n", ""),
        ("console.log(Object.assign([1,2], {extra:'v'}), Object.assign([], {extra:'v'}))", "[ 1, 2, extra: 'v' ] [ extra: 'v' ]\n", ""),
        ("console.log([1,,3], new Array(5), [,'a'])", "[ 1, <1 empty item>, 3 ] [ <5 empty items> ] [ <1 empty item>, 'a' ]\n", ""),
        ("console.log({a:1,b:2,c:3,d:4,e:5,f:6,g:7,h:8,i:9,j:10,k:11,l:12,m:13,n:14,o:15,p:16,q:17,r:18,s:19,t:20,u:21,v:22,w:23,x:24,y:25,z:26,aa:27,bb:28})", "{\n  a: 1,\n  b: 2,\n  c: 3,\n  d: 4,\n  e: 5,\n  f: 6,\n  g: 7,\n  h: 8,\n  i: 9,\n  j: 10,\n  k: 11,\n  l: 12,\n  m: 13,\n  n: 14,\n  o: 15,\n  p: 16,\n  q: 17,\n  r: 18,\n  s: 19,\n  t: 20,\n  u: 21,\n  v: 22,\n  w: 23,\n  x: 24,\n  y: 25,\n  z: 26,\n  aa: 27,\n  bb: 28\n}\n", ""),
        ("console.log({arr:Array.from({length:40},(_, i)=>i), o:{deep:{deeper:{deepest:1}}}})", "{\n  arr: [\n     0,  1,  2,  3,  4,  5,  6,  7,  8,  9,\n    10, 11, 12, 13, 14, 15, 16, 17, 18, 19,\n    20, 21, 22, 23, 24, 25, 26, 27, 28, 29,\n    30, 31, 32, 33, 34, 35, 36, 37, 38, 39\n  ],\n  o: { deep: { deeper: [Object] } }\n}\n", ""),
        ("console.log([{a:1,b:2},{a:3,b:4}], [[1,2],[3,4]], {list:[{x:1,y:[1,2,{z:3}]}]})", "[ { a: 1, b: 2 }, { a: 3, b: 4 } ] [ [ 1, 2 ], [ 3, 4 ] ] { list: [ { x: 1, y: [Array] } ] }\n", ""),
        ("console.log(Array.from({length:7},(_, i)=>'日本語'+i))", "[ '日本語0', '日本語1', '日本語2', '日本語3', '日本語4', '日本語5', '日本語6' ]\n", ""),
        ("console.log('é ü 日本語 😀', ['é', '日本語', '😀'])", "é ü 日本語 😀 [ 'é', '日本語', '😀' ]\n", ""),
        ("console.log('%s|%s|%s|%s', 'a', 1, -0, 1n)", "a|1|-0|1n\n", ""),
        ("console.log('%s %s %s %s', null, undefined, Symbol('q'), true)", "null undefined Symbol(q) true\n", ""),
        ("console.log('%s', {a:{b:{c:1}}}, '%s', [1,[2,[3]]])", "{ a: [Object] } %s [ 1, [ 2, [ 3 ] ] ]\n", ""),
        ("console.log('%d|%d|%d|%d|%d', '42', '4.5abc', -0, 1n, null)", "42|NaN|-0|1n|0\n", ""),
        ("console.log('%i|%i|%i|%i|%i', '42.9', -0.5, '0x1f', 'abc', 1e21)", "42|-0|31|NaN|1\n", ""),
        ("console.log('%f|%f|%f|%f|%f', '1.5e3x', '.5', 'Infinity', 'abc', -0)", "1500|0.5|Infinity|NaN|0\n", ""),
        ("console.log('%j %j %j %j', 'str', 1, null, undefined)", "\"str\" 1 null undefined\n", ""),
        ("console.log('%O', {a:{b:{c:{d:1}}}})", "{ a: { b: { c: [Object] } } }\n", ""),
        ("console.log('%c%s', 'color:red', 'v', 'extra', {a:1})", "v extra { a: 1 }\n", ""),
        ("console.log('%%', 1); console.log('100%'); console.log('%', 1); console.log('a%sb%sc', 1); console.log('%s%%%s', 1, 2)", "% 1\n100%\n% 1\na1b%sc\n1%2\n", ""),
        ("console.log('%s:%s', 'only')", "only:%s\n", ""),
        ("console.log('no directive', 1, 'two', {a:1})", "no directive 1 two { a: 1 }\n", ""),
        ("console.log(1, '%s', 2)", "1 %s 2\n", ""),
        ("console.log()", "\n", ""),
        ("console.log('')", "\n", ""),
        ("console.log('%d%d%d', 1, 2)", "12%d\n", ""),
        ("console.error('to stderr', {a:1}); console.warn('%s w', 'x'); console.info({i:1}); console.debug([1])", "{ i: 1 }\n[ 1 ]\n", "to stderr { a: 1 }\nx w\n"),
    ];
    for (source, stdout, stderr) in cases {
        let (out, err) = run(source);
        assert_eq!(&out, stdout, "stdout of {source}");
        assert_eq!(&err, stderr, "stderr of {source}");
    }
}

#[test]
fn logging_does_not_materialise_stdio_streams() {
    let (out, _) = run(r#"
        console.log("a", 1, { b: [1, 2] });
        console.error("e");
        const ops = process._console;
        console.log(ops.live(1), ops.live(2));
        process.stdout;
        console.log(ops.live(1), ops.live(2));
        "#);
    assert_eq!(out, "a 1 { b: [ 1, 2 ] }\nfalse false\ntrue false\n");
}

#[test]
fn console_and_stdout_writes_stay_ordered() {
    let (out, err) = run(r#"
        console.log("1");
        process.stdout.write("2\n");
        console.log({ three: 3 });
        process.stdout.write("4 no newline");
        console.log(" 5");
        console.error("e1");
        process.stderr.write("e2\n");
        console.error({ e: 3 });
        process.on("exit", () => { console.error("bye"); process.stderr.write("bye2\n"); });
        "#);
    assert_eq!(out, "1\n2\n{ three: 3 }\n4 no newline 5\n");
    assert_eq!(err, "e1\ne2\n{ e: 3 }\nbye\nbye2\n");
}

#[test]
fn large_interleaved_output_keeps_order() {
    let (out, _) = run(r#"
        for (let i = 0; i < 20000; i++) {
          if (i % 3 === 0) process.stdout.write("w" + i + "\n"); else console.log("l", i, { i });
        }
        process.stdout.write("tail");
        "#);
    let mut expected = String::new();
    for i in 0..20000 {
        if i % 3 == 0 {
            expected.push_str(&format!("w{i}\n"));
        } else {
            expected.push_str(&format!("l {i} {{ i: {i} }}\n"));
        }
    }
    expected.push_str("tail");
    assert_eq!(out, expected);
}

#[test]
fn patched_stdout_write_captures_console_output() {
    let (out, _) = run(r#"
        const write = process.stdout.write;
        const seen = [];
        process.stdout.write = (chunk) => { seen.push(String(chunk)); return true; };
        console.log("hidden", { a: 1 });
        console.info("hidden too");
        process.stdout.write = write;
        console.log(JSON.stringify(seen));
        "#);
    assert_eq!(out, "[\"hidden { a: 1 }\\n\",\"hidden too\\n\"]\n");
}

#[test]
fn replaced_stdout_property_receives_console_output() {
    let (out, err) = run(r#"
        const log = console.log;
        Object.defineProperty(process, "stdout", {
          value: { write(text) { process.stderr.write("fake:" + text); return true; } },
          configurable: true,
        });
        log("x", 1);
        "#);
    assert_eq!(out, "");
    assert_eq!(err, "fake:x 1\n");
}

#[test]
fn groups_indent_and_unwind() {
    let (out, _) = run(r#"
        console.group("g");
        console.log({ a: 1 }, "line\nbreak");
        console.group();
        console.log("deeper");
        console.groupEnd();
        console.groupEnd();
        console.log("out");
        "#);
    assert_eq!(out, "g\n  { a: 1 } line\n  break\n    deeper\nout\n");
}

#[test]
fn values_the_native_formatter_declines_still_print_like_node() {
    let (out, _) = run(r#"
        class P { constructor() { this.x = 1; } [Symbol.for("nodejs.util.inspect.custom")]() { return "custom!"; } }
        console.log(new P(), new Map([[1, { a: 2 }]]), new Set([1]), [1, , 3], { f() {} }.f.name);
        const circular = { name: "c" }; circular.me = circular;
        console.log(circular);
        console.log("%o", 1);
        "#);
    assert_eq!(
        out,
        "custom! Map(1) { 1 => { a: 2 } } Set(1) { 1 } [ 1, <1 empty item>, 3 ] f\n<ref *1> { name: 'c', me: [Circular *1] }\n1\n"
    );
}

#[test]
fn changing_inspect_default_options_applies_to_console() {
    let (out, _) = run(r#"
        console.log({ a: { b: { c: { d: 1 } } } });
        require("util").inspect.defaultOptions.depth = 0;
        console.log({ a: { b: 1 } });
        "#);
    assert_eq!(out, "{ a: { b: { c: [Object] } } }\n{ a: [Object] }\n");
}
