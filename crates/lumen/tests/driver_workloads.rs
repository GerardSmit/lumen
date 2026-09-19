//! Engine behaviours a real Node driver (Playwright over `ws`, a VS Code extension host) tripped
//! over: garbage collection from inside a coroutine, the regex backtracker on long inputs, an
//! unbounded microtask queue and V8's null-access wording. Each was found by running the driver,
//! then reduced to the smallest script that still failed. (The async context carried along
//! promise reactions is covered through `AsyncLocalStorage` in lumen-runtime's tests.)
use lumen::{bytecode::Tier, Completion, Engine};

fn check_with(tier: Tier, source: &str) {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    let script =
        format!("function assert(x, m) {{ if (!x) throw new Error(m || 'assertion'); }}\n{source}\n'passed'");
    match engine.eval(&script, false).unwrap() {
        Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
        Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
    }
}

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        check_with(tier, source);
    }
}

/// A generator body runs on a coroutine thread. The cycle collector used to keep its registry in
/// a thread-local, so a collection triggered inside the body saw an empty registry and freed
/// objects the driver thread still referenced (`inc_strong` on a dead cell, then a segfault).
#[test]
fn collection_inside_a_coroutine_sees_the_whole_heap() {
    for tier in [Tier::Interp, Tier::Bytecode] {
        check_with(
            tier,
            r#"
            const keep = [];
            for (let i = 0; i < 1000; i++) keep.push({ i, next: null });
            for (let i = 1; i < keep.length; i++) keep[i - 1].next = keep[i];
            function* churn() {
              for (let round = 0; round < 3; round++) {
                let cycle = null;
                for (let i = 0; i < 120_000; i++) { const o = { prev: cycle }; if (cycle) cycle.next = o; cycle = o; }
                yield round;
              }
            }
            for (const round of churn()) assert(typeof round === "number");
            let sum = 0;
            for (let node = keep[0]; node; node = node.next) sum += node.i;
            assert(sum === 499500, "driver-thread objects survived the coroutine's collection");
        "#,
        );
    }
}

/// Backtracking recursion is capped so a pathological pattern cannot overflow the stack, but a
/// benign pattern over a long input (a 60 KB base64 body, a CDP message) needs one frame per
/// character. The matcher retries on a thread with a deep stack instead of returning "no match".
#[test]
fn regex_retries_long_inputs_on_a_deep_stack() {
    // The capped attempt alone needs more than a test thread's 2 MiB; the CLI runs on a bigger
    // main stack, so the test does too.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(regex_long_inputs)
        .unwrap()
        .join()
        .unwrap();
}

fn regex_long_inputs() {
    check(
        r#"
        const body = "a".repeat(60_000) + "=";
        assert(/^([A-Za-z0-9+/]*)=*$/.test(body), "long benign match");
        const m = /^(?:x|y)+$/.exec("xy".repeat(20_000));
        assert(m && m[0].length === 40_000, "alternation over a long input");
        assert("q".repeat(50_000).replace(/(q)/g, "$1$1").length === 100_000, "global replace");
        assert(/^(a+)+b$/.test("a".repeat(40)) === false, "pathological pattern still terminates");
    "#,
    );
}

/// Node drains the microtask queue until it is empty; a budget that stopped early left promise
/// chains longer than the budget unresolved, which read as a hang in a 100k-frame stress test.
#[test]
fn microtask_queue_drains_to_empty() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        // `eval` runs the microtask checkpoint after the script, so `done` must hold afterwards.
        engine
            .eval(
                r#"
                var count = 0, done = false;
                let p = Promise.resolve();
                for (let i = 0; i < 150_000; i++) p = p.then(() => { count++; });
                p.then(() => { done = true; });
            "#,
                false,
            )
            .unwrap();
        match engine
            .eval("done ? 'passed' : 'unresolved after ' + count", false)
            .unwrap()
        {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

/// Playwright matches the engine's own error text (`/^TypeError: Cannot read properties of null/`)
/// to classify a failure, so the wording has to be V8's, including the property name.
#[test]
fn null_access_errors_use_v8_wording() {
    check(
        r#"
        const messages = [];
        const attempts = [
          () => null.foo,
          () => undefined["bar"],
          () => { const key = "baz"; return null[key]; },
          () => { null.qux = 1; },
          () => undefined[0],
        ];
        for (const attempt of attempts) {
          try { attempt(); assert(false, "must throw"); } catch (e) { assert(e instanceof TypeError); messages.push(e.message); }
        }
        assert(messages[0] === "Cannot read properties of null (reading 'foo')", messages[0]);
        assert(messages[1] === "Cannot read properties of undefined (reading 'bar')", messages[1]);
        assert(messages[2] === "Cannot read properties of null (reading 'baz')", messages[2]);
        assert(messages[3] === "Cannot set properties of null (setting 'qux')", messages[3]);
        assert(messages[4] === "Cannot read properties of undefined (reading '0')", messages[4]);
    "#,
    );
}
