use super::switch_tests::evaluate;
use crate::{bytecode::Tier, Completion, Engine, JitMode};
use std::time::Duration;

fn background_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Bytecode);
    engine.set_tier_threshold(0);
    engine.set_jit_mode(JitMode::Hot);
    engine.set_jit_background(true);
    engine
}

fn run(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

fn interpreted(src: &str) -> String {
    evaluate(src, Tier::Interp, JitMode::Disabled).0
}

/// Runs `call` until native code was installed and entered, comparing each result with `want`.
fn until_native(engine: &mut Engine, setup: &str, call: &str, want: &str) {
    run(engine, setup);
    for _ in 0..400 {
        assert_eq!(run(engine, call), want);
        let stats = engine.jit_stats();
        if stats.compiled_units > 0 && stats.executed_entries > 0 {
            assert_eq!(run(engine, call), want);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no native code was installed: {:?}", engine.jit_stats());
}

#[test]
fn a_hot_loop_installs_in_the_background_and_agrees() {
    let setup = "function lp(n){var s=0;for(var i=0;i<n;i++){s=(s+i*3)%1000003}return s}";
    let call = "lp(50000)";
    let want = interpreted(&format!("{setup};{call}"));
    until_native(&mut background_engine(), setup, call, &want);
}

#[test]
fn a_hot_function_installs_in_the_background_and_agrees() {
    let setup = "function fib(n){return n<2?n:fib(n-1)+fib(n-2)}";
    let call = "fib(16)";
    let want = interpreted(&format!("{setup};{call}"));
    until_native(&mut background_engine(), setup, call, &want);
}

#[test]
fn a_float_function_installs_in_the_background_and_agrees() {
    let setup = "function mix(a,b){return a*0.5+b/3}
                 function pump(n){var t=0;for(var i=0;i<n;i++){t+=mix(i,i+1)}return Math.round(t)}";
    let call = "pump(4000)";
    let want = interpreted(&format!("{setup};{call}"));
    until_native(&mut background_engine(), setup, call, &want);
}

#[test]
fn replacing_a_guarded_global_while_compiles_are_pending_stays_correct() {
    let setup = "function g(n){var t=0;for(var i=0;i<n;i++){t+=Math.floor(i/3)}return t}
                 var saved=Math.floor;";
    let steps = |k: usize| match k {
        3 => "Math.floor=function(x){return 1}",
        9 => "Math.floor=saved",
        14 => "Object.defineProperty(Math,'floor',{get:function(){return function(x){return 2}},configurable:true})",
        20 => "Object.defineProperty(Math,'floor',{value:saved,writable:true,configurable:true})",
        _ => "g(20000)",
    };
    let mut want = Vec::new();
    {
        let mut reference = Engine::new();
        reference.set_jit_mode(JitMode::Disabled);
        run(&mut reference, setup);
        for k in 0..30 {
            want.push(run(&mut reference, steps(k)));
        }
    }
    let mut engine = background_engine();
    run(&mut engine, setup);
    for (k, expected) in want.iter().enumerate() {
        assert_eq!(&run(&mut engine, steps(k)), expected, "step {k}");
        std::thread::sleep(Duration::from_millis(3));
    }
}

#[test]
fn resetting_native_code_while_compiles_are_pending_recompiles_later() {
    let setup = "function lp(n){var s=0;for(var i=0;i<n;i++){s=(s+i*3)%1000003}return s}
                 function fib(n){return n<2?n:fib(n-1)+fib(n-2)}";
    let want_lp = interpreted(&format!("{setup};lp(30000)"));
    let want_fib = interpreted(&format!("{setup};fib(15)"));
    let mut engine = background_engine();
    run(&mut engine, setup);
    for round in 0..40 {
        assert_eq!(run(&mut engine, "lp(30000)"), want_lp);
        assert_eq!(run(&mut engine, "fib(15)"), want_fib);
        if round % 3 == 0 {
            // SAFETY: the only engine on this thread is idle between evals.
            unsafe { super::evict_quiescent_code() };
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..400 {
        assert_eq!(run(&mut engine, "lp(30000)"), want_lp);
        assert_eq!(run(&mut engine, "fib(15)"), want_fib);
        if engine.jit_stats().compiled_units > 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("nothing was installed after the resets: {:?}", engine.jit_stats());
}

#[test]
fn dropping_the_engine_with_compiles_in_flight_is_safe() {
    let setup = "function lp(n){var s=0;for(var i=0;i<n;i++){s=(s+i*3)%1000003}return s}
                 function fib(n){return n<2?n:fib(n-1)+fib(n-2)}";
    let want = interpreted(&format!("{setup};lp(20000)+':'+fib(14)"));
    for _ in 0..25 {
        let mut engine = background_engine();
        run(&mut engine, setup);
        assert_eq!(run(&mut engine, "lp(20000)+':'+fib(14)"), want);
        drop(engine);
    }
    std::thread::sleep(Duration::from_millis(50));
    let mut engine = background_engine();
    run(&mut engine, setup);
    assert_eq!(run(&mut engine, "lp(20000)+':'+fib(14)"), want);
}
