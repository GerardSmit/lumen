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
        assert!(after < Duration::from_millis(500), "{src}: ran {after:?} past the interrupt");
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
        assert!(after < Duration::from_millis(500), "{src}: ran {after:?} past the interrupt");
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
        assert!(after < Duration::from_secs(2), "{src}: ran {after:?} past the interrupt");
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
