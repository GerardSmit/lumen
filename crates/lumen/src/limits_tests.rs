use crate::{Completion, Engine};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Run `src` with an interrupt raised after `delay`; returns how long the engine kept running
/// after the flag was set, and the completion.
fn run_interrupted(src: &'static str, delay: Duration) -> (Duration, Completion) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let mut engine = Engine::new();
            let flag = Arc::new(AtomicBool::new(false));
            engine.set_interrupt(Arc::clone(&flag));
            let setter = std::thread::spawn(move || {
                std::thread::sleep(delay);
                flag.store(true, Ordering::SeqCst);
                Instant::now()
            });
            let completion = engine.eval(src, false).expect("parse");
            let done = Instant::now();
            let set_at = setter.join().unwrap();
            (done.saturating_duration_since(set_at), completion)
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn interrupt_stops_native_array_like_loops() {
    let cases = [
        "Array.prototype.indexOf.call({length: 2**53 - 1}, 1)",
        "Array.prototype.lastIndexOf.call({length: 2**53 - 1}, 1)",
        "Array.prototype.includes.call({length: 2**53 - 1}, 1)",
        "Array.prototype.forEach.call({length: 2**53 - 1}, function () {})",
        "Array.prototype.reverse.call({length: 2**53 - 1})",
        "Array.prototype.copyWithin.call({length: 2**53 - 1}, 1, 0)",
        "Array.prototype.unshift.call({length: 2**53 - 2}, 1)",
        "new Array(2**32 - 1).indexOf(1)",
        "String.raw({raw: {length: 2**32}})",
        "Math.max.apply(null, {length: 1e8})",
        "Reflect.apply(Math.max, null, {length: 1e8})",
    ];
    for src in cases {
        let (after, completion) = run_interrupted(src, Duration::from_millis(30));
        if let Completion::Value(v) = &completion {
            panic!("{src}: expected the termination, got {v}");
        }
        // Debug builds are slow; the poll stride keeps this well under a second regardless.
        assert!(
            after < Duration::from_millis(500),
            "{src}: ran {after:?} past the interrupt"
        );
    }
}

#[test]
fn interrupt_stops_a_blocked_atomics_wait() {
    let cases = [
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0)",
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1e9)",
    ];
    for src in cases {
        let (after, completion) = run_interrupted(src, Duration::from_millis(100));
        if let Completion::Value(v) = &completion {
            panic!("{src}: expected the termination, got {v}");
        }
        assert!(
            after < Duration::from_millis(500),
            "{src}: ran {after:?} past the interrupt"
        );
    }
}

#[test]
fn heap_limit_is_inert_without_the_class_allocator() {
    // The test binary does not install `ClassAlloc`: no byte count, so no false positives.
    let mut engine = Engine::new();
    engine.set_heap_limit(1);
    if Engine::heap_bytes().is_none() {
        assert!(matches!(
            engine.eval("[1, 2, 3].map(x => x * 2).join()", false).unwrap(),
            Completion::Value(v) if v == "2,4,6"
        ));
    }
}

#[test]
fn interrupt_stops_long_bigint_operations() {
    let cases = [
        "(3n ** 6000000n).toString()",
        "const a = 7n ** 3000000n; a * a",
        "const a = 7n ** 2000000n; (a * a * a) / (a + 1n)",
        "BigInt('9'.repeat(8000000))",
        "const a = 5n ** 3000000n; a.toString(7)",
    ];
    for src in cases {
        let (after, completion) = run_interrupted(src, Duration::from_millis(50));
        if let Completion::Value(v) = &completion {
            panic!("{src}: expected the termination, got {v}");
        }
        assert!(
            after < Duration::from_secs(2),
            "{src}: ran {after:?} past the interrupt"
        );
    }
}

#[test]
fn bigint_update_operators_respect_the_size_cap() {
    let mut engine = Engine::new();
    let src = "
        let y = 1n << 1073741823n;
        y = y + (y - 1n);
        const out = [];
        try { y++; out.push('no error'); } catch (e) { out.push(e.name + ':' + e.message); }
        try { ++y; out.push('no error'); } catch (e) { out.push(e.name); }
        function f(z) { try { z++; return 'no error'; } catch (e) { return e.name; } }
        out.push(f(y));
        let neg = -y;
        try { neg--; out.push('no error'); } catch (e) { out.push(e.name); }
        out.join('|')
    ";
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => assert_eq!(
            v.to_string(),
            "RangeError:Maximum BigInt size exceeded|RangeError|RangeError|RangeError"
        ),
        _ => panic!("threw"),
    }
}

struct Yielded {
    completion: Completion,
    yields: usize,
    jit_units: u64,
}

/// Run `src` with a yield hook that re-arms its flag, so every safe point yields. After
/// `interrupt_after` yields the hook raises the hard interrupt. With `interrupt_first` the
/// interrupt is raised before the run starts.
fn run_yielding(
    src: &'static str,
    tier: crate::bytecode::Tier,
    jit: crate::JitMode,
    interrupt_after: Option<usize>,
    interrupt_first: bool,
) -> Yielded {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_jit_mode(jit);
            engine.set_tier_threshold(0);
            let interrupt = Arc::new(AtomicBool::new(interrupt_first));
            let flag = Arc::new(AtomicBool::new(true));
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            if interrupt_after.is_some() || interrupt_first {
                engine.set_interrupt(Arc::clone(&interrupt));
            }
            let (f, c, i) = (
                Arc::clone(&flag),
                Arc::clone(&count),
                Arc::clone(&interrupt),
            );
            engine.set_yield_hook(
                Arc::clone(&flag),
                Arc::new(move || {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if interrupt_after == Some(n) {
                        i.store(true, Ordering::SeqCst);
                    }
                    f.store(true, Ordering::SeqCst);
                }),
            );
            let completion = engine.eval(src, false).expect("parse");
            Yielded {
                completion,
                yields: count.load(Ordering::SeqCst),
                jit_units: engine.jit_stats().compiled_units,
            }
        })
        .unwrap()
        .join()
        .unwrap()
}

const YIELD_TIERS: [(crate::bytecode::Tier, crate::JitMode); 3] = [
    (crate::bytecode::Tier::Interp, crate::JitMode::Disabled),
    (crate::bytecode::Tier::Bytecode, crate::JitMode::Disabled),
    (crate::bytecode::Tier::Bytecode, crate::JitMode::Eager),
];

#[test]
fn yield_hook_resumes_a_counting_loop_on_every_tier() {
    let src = "var n = 0; var f = (function () { var c = 0; return () => ++c; })();
        function run() {
            for (var i = 0; i < 300000; i++) { n += 1; f(); }
            return n === 300000 && f() === 300001 && i === 300000;
        }
        run()";
    for (tier, jit) in YIELD_TIERS {
        let out = run_yielding(src, tier, jit, None, false);
        assert!(
            matches!(&out.completion, Completion::Value(v) if v == "true"),
            "{tier:?}/{jit:?}: {:?}",
            describe(&out.completion)
        );
        assert!(
            out.yields >= 10,
            "{tier:?}/{jit:?}: only {} yields",
            out.yields
        );
        if jit == crate::JitMode::Eager {
            eprintln!("jit units compiled: {}", out.jit_units);
        }
    }
}

#[test]
fn hard_interrupt_ends_a_yielding_infinite_loop() {
    let src = "var n = 0; function run() { while (true) { n++; } } run()";
    for (tier, jit) in YIELD_TIERS {
        let out = run_yielding(src, tier, jit, Some(5), false);
        assert!(
            !matches!(out.completion, Completion::Value(_)),
            "{tier:?}/{jit:?}: {:?}",
            describe(&out.completion)
        );
        assert_eq!(out.yields, 5, "{tier:?}/{jit:?}");
    }
}

#[test]
fn pending_interrupt_wins_over_a_yield() {
    let src = "function run() { while (true) {} } run()";
    for (tier, jit) in YIELD_TIERS {
        let out = run_yielding(src, tier, jit, None, true);
        assert!(
            !matches!(out.completion, Completion::Value(_)),
            "{tier:?}/{jit:?}"
        );
        assert_eq!(out.yields, 0, "{tier:?}/{jit:?}");
    }
}

#[test]
fn yield_hook_can_be_removed() {
    let mut engine = Engine::new();
    let flag = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    engine.set_yield_hook(
        Arc::clone(&flag),
        Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }),
    );
    engine.clear_yield_hook();
    let out = engine.eval(
        "var s = 0; for (var i = 0; i < 50000; i++) { s += i; } s",
        false,
    );
    assert!(matches!(out, Ok(Completion::Value(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn describe(c: &Completion) -> String {
    match c {
        Completion::Value(v) => format!("value {v}"),
        Completion::Throw { name, message } => format!("throw {name}: {message}"),
    }
}
