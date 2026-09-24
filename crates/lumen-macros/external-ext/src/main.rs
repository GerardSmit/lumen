//! Runs the extension from JS and checks every conversion, the error cases, the classes and the
//! promise paths. Exits non-zero on the first mismatch.

use lumen::embed::Value;
use lumen::Engine;
use lumen_ext_demo::{install, settle_pending, RESPONSES_DROPPED};

/// Evaluate `src`; render the completion (or the thrown error as `Name: message`).
fn js(e: &mut Engine, src: &str) -> String {
    let wrapped = format!(
        "(() => {{ try {{ return String((() => {{ {src} }})()); }} \
         catch (err) {{ return 'THROW ' + (err && err.name) + ': ' + (err && err.message); }} }})()"
    );
    match e.eval_value(&wrapped).expect("parse error") {
        Ok(Value::Str(s)) => s.to_string(),
        Ok(_) => "<non-string>".into(),
        Err(_) => "<uncaught>".into(),
    }
}

fn check(e: &mut Engine, src: &str, want: &str) {
    let got = js(e, src);
    if got != want {
        eprintln!("FAIL: {src}\n  want: {want}\n  got:  {got}");
        std::process::exit(1);
    }
    println!("ok  {src}  =>  {got}");
}

fn check_contains(e: &mut Engine, src: &str, want: &str) {
    let got = js(e, src);
    if !got.contains(want) {
        eprintln!("FAIL: {src}\n  want substring: {want}\n  got:  {got}");
        std::process::exit(1);
    }
    println!("ok  {src}  =>  {got}");
}

fn main() {
    let mut e = Engine::new();
    install(&mut e);

    // Numbers.
    check(&mut e, "return ext.clamp(5, 0, 3)", "3");
    check(&mut e, "return ext.clamp.length + ' ' + ext.clamp.name", "3 clamp");
    check(&mut e, "return ext.clamp('5', 0, 3)",
        "THROW TypeError: clamp: argument 1 (x) must be a number");
    check(&mut e, "return ext.clamp(1)",
        "THROW TypeError: clamp: argument 2 (lo) must be a number");
    check(&mut e, "return ext.ints(2**32 + 5, -1, 2**53 - 1, 10n).join()",
        "5,4294967295,9007199254740991,11");
    check(&mut e, "return ext.ints(1, 1, 1.5, 1n)",
        "THROW RangeError: ints: argument 3 (c) must be a safe integer");
    check(&mut e, "return ext.ints(1, 1, 7n, -1n)",
        "THROW RangeError: ints: argument 4 (d) is out of range");
    check(&mut e, "return typeof ext.big(21n) + ' ' + ext.big(21n)", "bigint 42");
    check_contains(&mut e, "return ext.tooBig ?? ext.too_big()", "THROW RangeError: result 9223372036854775807 exceeds");
    check(&mut e, "return ext.coerced('4.5', 12, '')", "4.5|12|false");
    check(&mut e, "return ext.coerced({ valueOf() { return 7 } }, null, {})", "7|null|true");

    // Strings.
    check(&mut e, "return ext.greet('lumen')", "Hello, lumen!");
    check(&mut e, "return ext.greet('lumen', 'Hi') + ' ' + ext.greet.length", "Hi, lumen! 1");
    check(&mut e, "return ext.greet('x', undefined)", "Hello, x!");
    check(&mut e, "return ext.greet(42)", "THROW TypeError: greet: argument 1 (name) must be a string");
    check(&mut e, "return ext.shout('héllo')", "HÉLLO");
    check(&mut e, "return ext.words(' a  bb ccc ').join('|')", "a|bb|ccc");

    // Bytes.
    check(&mut e, "return ext.sum(new Uint8Array([1, 2, 3, 250]))", "256");
    check(&mut e, "return ext.sum(new Uint8Array([9, 1, 2, 9]).subarray(1, 3))", "3");
    check(&mut e, "return ext.sum(new Uint16Array([0x0101]).buffer)", "2");
    check(&mut e, "return ext.sum(new DataView(new Uint8Array([5, 6, 7]).buffer, 1))", "13");
    check(&mut e, "return ext.sum([1, 2])",
        "THROW TypeError: sum: argument 1 (bytes) must be an ArrayBuffer, a TypedArray (Uint8Array, Buffer, ...) or a DataView");
    check(&mut e, "const u = new Uint8Array(8); u.buffer.transfer(); return ext.sum(u)",
        "THROW TypeError: sum: argument 1 (bytes) is backed by a detached ArrayBuffer");
    check(&mut e, "const b = new ArrayBuffer(4); b.transfer(); return ext.blen(b)",
        "THROW TypeError: blen: argument 1 (bytes) is backed by a detached ArrayBuffer");
    check(&mut e, "const r = ext.xorBytes ?? ext.xor_bytes; const out = r(true, new Uint8Array([1]), new Uint8Array([1, 2, 3])); \
         return (out instanceof Uint8Array) + ' ' + out.join() + ' ' + out.buffer.byteLength",
        "true 0,3,2 3");
    check(&mut e, "try { ext.xor_bytes(true, new Uint8Array(0), new Uint8Array(1)) } catch (e) { return e.name + ' ' + e.code }",
        "RangeError ERR_CRYPTO_INVALID_KEYLEN");
    check(&mut e, "const d = new Uint8Array(4); const n = ext.copy_into(d, new Uint8Array([7, 8])); return n + ':' + d.join()",
        "2:7,8,0,0");
    check(&mut e, "const q = new Uint8Array(8); q[4] = 1; ext.copy_into(q.subarray(0, 4), q.subarray(4)); return q.join()",
        "1,0,0,0,1,0,0,0");
    check(&mut e, "const q = new Uint8Array(8); return ext.copy_into(q, q.subarray(2))",
        "THROW TypeError: copy_into: argument 2 (src) overlaps argument 1 (dst) in the same buffer; a mutable byte slice cannot alias");
    check(&mut e, "const q = new Uint8Array(8); return ext.copy_into(q.subarray(0, 4), q.subarray(3))",
        "THROW TypeError: copy_into: argument 2 (src) overlaps argument 1 (dst) in the same buffer; a mutable byte slice cannot alias");
    check(&mut e, "const s = new SharedArrayBuffer(4); new Uint8Array(s).fill(2); return ext.sum(new Uint8Array(s))", "8");
    check(&mut e, "return ext.copy_into(new Uint8Array(new SharedArrayBuffer(4)), new Uint8Array(1))",
        "THROW TypeError: copy_into: argument 1 (dst) must not be a SharedArrayBuffer view (&mut [u8])");
    check(&mut e,
        "const d = new Uint8Array(4); const seen = ext.fill_with_callback(d, () => d.length); return seen + ':' + d.length + ':' + d.join()",
        "0:4:7,7,7,7");
    check(&mut e,
        "const d = new Uint8Array(4); return ext.fill_with_callback(d, () => { d.buffer.transfer(); return 1; })",
        "THROW TypeError: ArrayBuffer is detached");
    check(&mut e, "return ext.owned_copy(new Float64Array(3))", "24");
    check(&mut e, "const b = ext.make_buffer(3); return (b instanceof ArrayBuffer) + ' ' + new Uint8Array(b).join()", "true 1,1,1");

    // Objects, functions, arrays, this, state, Option, Value.
    check(&mut e, "return ext.call_twice(x => x * 3, 2)", "18");
    check(&mut e, "return ext.call_twice(() => { throw new SyntaxError('boom') }, 2)", "THROW SyntaxError: boom");
    check(&mut e, "return ext.call_twice(1, 2)", "THROW TypeError: call_twice: argument 1 (f) must be a function");
    check(&mut e, "return ext.get_field({ a: { b: 1 } }, 'a').b", "1");
    check(&mut e, "return ext.get_field('str', 'a')", "THROW TypeError: get_field: argument 1 (obj) must be an object");
    check(&mut e, "return ext.vec_sum([1, 2, 3.5])", "6.5");
    check(&mut e, "return ext.vec_sum([1, 'x'])", "THROW TypeError: vec_sum: argument 1 (xs) element 1 must be a number");
    check(&mut e, "return ext.vec_sum({ length: 1, 0: 1 })", "THROW TypeError: vec_sum: argument 1 (xs) must be an array");
    check(&mut e, "return JSON.stringify(ext.range(4))", "[0,1,2,3]");
    check(&mut e, "return ext.this_is_object() + ' ' + ext.this_is_object.call({})", "true true");
    check(&mut e, "const f = ext.this_is_object; return f()", "false");
    check(&mut e, "const o = {}; return ext.identity(o) === o", "true");
    check(&mut e, "ext.counter_add(2); return ext.counter_add(3)", "5");
    check(&mut e, "return ext.maybe() + ' ' + ext.maybe(null) + ' ' + ext.maybe(4)", "null null 8");
    check(&mut e, "return ext.parse_int(' 12 ')", "12");
    check(&mut e, "return ext.parse_int('x')", "THROW SyntaxError: invalid digit found in string");

    // Classes.
    check(&mut e, "const r = new Response('hi', 201); return [r.status, r.ok, r instanceof Response, typeof r.text].join()",
        "201,true,true,function");
    check(&mut e, "return Object.prototype.toString.call(new Response())", "[object Response]");
    check(&mut e, "return Response.length + ' ' + Response.name", "0 Response");
    check(&mut e, "return new Response('x', 99)", "THROW RangeError: status 99 out of range");
    check(&mut e, "return Response('x')", "THROW TypeError: Class constructor Response cannot be invoked without 'new'");
    check(&mut e, "return Response.prototype.text.call({})",
        "THROW TypeError: Response.text: illegal invocation (receiver is not a Response)");
    check(&mut e, "return Object.getOwnPropertyDescriptor(Response.prototype, 'status').get.call(new Headers())",
        "THROW TypeError: Illegal constructor");
    check(&mut e, "const h = new Response().headers; return Object.getOwnPropertyDescriptor(Response.prototype, 'status').get.call(h)",
        "THROW TypeError: Response.status: illegal invocation (receiver is not a Response)");
    check(&mut e, "const r = new Response(); r.status = 404; return r.status + ' ' + r.ok", "404 false");
    check(&mut e, "const r = new Response(); r.status = 'x'; return r.status",
        "THROW TypeError: Response.status: argument 1 (status) must be a number");
    check(&mut e, "const h = new Response('', 200).headers; return [h.get('Content-Type'), h.has('nope'), h.size, h instanceof Headers].join()",
        "text/plain,false,1,true");
    check(&mut e, "return new Headers()", "THROW TypeError: Illegal constructor");
    check(&mut e, "const r = Response.error(); return r.status + ' ' + (r instanceof Response)", "500 true");
    check(&mut e, "const b = new Response('abc').bytesSync(); return b.constructor.name + ' ' + b.join()", "Uint8Array 97,98,99");
    check(&mut e, "const r = new Response('', 200); return r.update(s => s + 1) + ' ' + r.status", "201 201");
    check(&mut e, "const r = new Response('', 200); return r.update(s => r.status)",
        "THROW TypeError: Response.status: receiver (this) is a Response already in use by an enclosing call (re-entrant borrow)");
    check(&mut e, "return ext.response_status(new Response('', 204))", "204");
    check(&mut e, "return ext.response_status({})", "THROW TypeError: response_status: argument 1 (r) must be a Response");
    check(&mut e,
        "class Mine extends Response { constructor() { super('sub', 202); } get extra() { return 'x' } } \
         const m = new Mine(); return [m.status, m.extra, m instanceof Mine, m instanceof Response, Object.getPrototypeOf(m) === Mine.prototype].join()",
        "202,x,true,true,true");

    // Promises: fetch() -> Response -> text()/json().
    let _ = js(&mut e, "globalThis.out = []; \
        fetch('https://example.test/data.json').then(r => { out.push(r.status, r.ok, r.headers.get('x-demo')); return r.json(); }) \
          .then(j => out.push(j.answer, j.list.length)); \
        fetch('https://example.test/hello').then(r => r.text()).then(t => out.push(t)); \
        fetch('https://example.test/missing').then(r => out.push(r.ok)); \
        fetch('nope://x').catch(err => out.push(err.name + ': ' + err.message)); \
        fetch(42).catch(err => out.push(err.message)); \
        new Response('abc').arrayBuffer().then(b => out.push(b.byteLength)); \
        return 'queued'");
    e.run_microtasks();
    check(&mut e, "return out.join('|')",
        "200|true|1|false|TypeError: fetch failed: unknown host in nope://x|fetch: argument 1 (url) must be a string|3|42|3|hello world");

    // A promise settled later by the host (the embedder's event loop).
    let _ = js(&mut e, "globalThis.late = []; ext.delayedDouble(21).then(v => late.push(v)); \
        ext.delayedDouble(-1).catch(e => late.push(e.name)); return ''");
    e.run_microtasks();
    check(&mut e, "return late.length", "0");
    assert_eq!(settle_pending(&mut e), 2);
    check(&mut e, "return late.join()", "42,RangeError");

    // Instance lifetime: the Rust value drops once its JS object is gone.
    let before = RESPONSES_DROPPED.with(|c| c.get());
    let _ = js(&mut e, "for (let i = 0; i < 1000; i++) { new Response('x'); } return ''");
    let _ = js(&mut e, "globalThis.cyc = new Response('c'); cyc.self = cyc; globalThis.cyc = undefined; return ''");
    let swept = e.ctx().collect_garbage();
    let dropped = RESPONSES_DROPPED.with(|c| c.get()) - before;
    println!("instances: {swept} swept by collect_garbage, {dropped} dropped in total");
    assert!(dropped >= 1001, "dropped {dropped}");

    // Fast-op descriptor lookup (what the JIT would use).
    let clamp_fn = {
        let g = e.global_this();
        let ext = e.ctx().get_member(&g, "ext").ok().unwrap();
        e.ctx().get_member(&ext, "clamp").ok().unwrap()
    };
    let sig = e.ctx().fast_op_of(&clamp_fn).expect("clamp is a fast op");
    assert_eq!(sig.args.len(), 3);
    let f: extern "C" fn(f64, f64, f64) -> f64 = unsafe { std::mem::transmute(sig.entry.0) };
    assert_eq!(f(9.0, 1.0, 4.0), 4.0);
    println!("fast entry: {:?} -> {:?}, direct call ok", sig.args, sig.ret);

    println!("\nall checks passed");
}
