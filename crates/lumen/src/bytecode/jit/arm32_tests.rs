//! 32-bit ARM end-to-end: native code for numbers, strings, objects, arrays and calls must
//! agree with the interpreter. (The shared JIT test modules run here too.)

use crate::{bytecode::Tier, Completion, Engine, JitMode, JitStats};

fn evaluate(src: &str, tier: Tier, mode: JitMode) -> (String, JitStats) {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(mode);
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => (v, engine.jit_stats()),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

/// `src` agrees on every tier; when `native`, the eager JIT also ran machine code.
fn agree(src: &str, native: bool) {
    let want = evaluate(src, Tier::Interp, JitMode::Disabled).0;
    for mode in [JitMode::Disabled, JitMode::Hot] {
        assert_eq!(evaluate(src, Tier::Bytecode, mode).0, want, "{mode:?}");
    }
    let (got, stats) = evaluate(src, Tier::Bytecode, JitMode::Eager);
    assert_eq!(got, want, "eager JIT");
    if native {
        assert!(stats.compiled_units > 0, "nothing was compiled: {stats:?}");
        assert!(stats.executed_entries > 0, "no native execution: {stats:?}");
    }
}

#[test]
fn integer_loops_run_natively() {
    agree(
        "function f(n){var s=0,t=1;for(var i=0;i<n;i++){s=(s+i*3)|0;t=(t*31+i)%1000003;}return s+':'+t}f(50000)",
        true,
    );
    agree(
        "function f(n){var x=123456789;for(var i=0;i<n;i++){x^=x<<13;x^=x>>>17;x^=x<<5;}return x}f(10000)",
        true,
    );
}

#[test]
fn float_loops_run_natively() {
    agree(
        "function f(n){var a=0.5,b=1.25;for(var i=0;i<n;i++){a=a*1.0001+b/3;b=b-a*0.001;}return a.toFixed(6)+b.toFixed(6)}f(20000)",
        true,
    );
}

#[test]
fn int32_conversions_of_large_and_special_doubles() {
    let src = "function f(n){var s=0;var v=[1e10,-1e10,2147483648,-2147483649,4294967296.5,1e300,NaN,Infinity,-0.5,9007199254740993];\
               for(var i=0;i<n;i++){s=(s+(i*1e9+0.5)|0)|0;}return s}f(5000)";
    agree(src, true);
    let src = "function f(n){var s=0;for(var i=0;i<n;i++){var x=i*4294967296.0+i;s=(s+(x|0)+(x>>>0))|0;}return s}f(3000)";
    agree(src, true);
}

#[test]
fn programs_with_heap_values_run_natively() {
    agree("function f(n){var s='';for(var i=0;i<n;i++){s+=i;}return s.length}f(2000)", true);
    agree("function f(n){var o={a:1,b:2};var s=0;for(var i=0;i<n;i++){s+=o.a+o.b;o.a=i;}return s}f(3000)", true);
    agree("function f(n){var a=[];for(var i=0;i<n;i++){a.push(i*2);}var s=0;for(var j=0;j<a.length;j++){s+=a[j];}return s}f(3000)", true);
    agree("function g(x){return x+1}function f(n){var s=0;for(var i=0;i<n;i++){s=g(s);}return s}f(3000)", true);
    agree("function fib(n){return n<2?n:fib(n-1)+fib(n-2)}fib(18)", true);
}

#[test]
fn handles_and_bools_survive_native_round_trips() {
    // Handles written by native code and read back by Rust (and the reverse), booleans in
    // slots, object identity in caches, and symbols/BigInts as plain payloads.
    agree("function f(n){var o={k:'v'},p=o,b=false,s=Symbol('x'),z=10n;var c=0;for(var i=0;i<n;i++){if(p===o)c++;b=!b;p=(i&1)?o:{k:'w'};z+=1n;}return c+':'+b+':'+p.k+':'+String(s)+':'+z}f(4001)", true);
    agree("function f(n){var a=['x','y'];var t='';for(var i=0;i<n;i++){t=a[i&1]+t.length;}return t}f(3000)", true);
}

#[test]
fn rounding_and_remainders_run_natively() {
    // Math.floor/ceil/round/trunc (inline on ARM32) over signed zeros, halves and large values.
    agree(
        "function f(n){var s=0,z='';for(var i=0;i<n;i++){var x=(i-n/2)*0.37;s+=Math.floor(x)+Math.ceil(x)*3+Math.round(x)*5+Math.trunc(x)*7;}\
         var v=[-0,0,-0.5,0.5,-0.7,2.5,-2.5,4503599627370495.5,-1e300,NaN,Infinity];\
         for(var j=0;j<v.length;j++){z+=[Math.floor(v[j]),Math.ceil(v[j]),Math.round(v[j]),Math.trunc(v[j])].map(function(y){return Object.is(y,-0)?'-0':String(y)}).join(',')+';'}\
         return s+':'+z}f(4000)",
        true,
    );
    // Int32 remainders by constants (powers of two, odd, negative) and by a variable.
    agree(
        "function f(n){var s=0;for(var i=-n;i<n;i++){s=(s+i%7+i%1000+i%-16+i%1000003+(i%8)*3)|0;s=(s+(i*7919)%(1+(i&63)))|0;}return s}f(5000)",
        true,
    );
}
