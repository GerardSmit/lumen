//! Compiled chunks outlive the pooled coroutine thread that first compiled them.
//! Different engines must keep their direct-call records independent when workers
//! are reused and their cached chunks execute on other physical threads.
use lumen::{bytecode::Tier, Completion, Engine};
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_engines_reuse_coroutines_without_aliasing_jit_frames() {
    let barrier = Arc::new(Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|owner| {
            let barrier = barrier.clone();
            std::thread::Builder::new()
                .stack_size(64 * 1024 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let mut engine = Engine::new();
                    engine.set_tier(Tier::Bytecode);
                    engine.set_tier_threshold(0);
                    let setup = format!(
                        r#"
                        globalThis.owner = {owner};
                        function leaf(n) {{ return {{ text: 'engine-' + owner + '-' + n, value: n + owner }}; }}
                        function middle(n) {{ const x = leaf(n); return x.value + x.text.length; }}
                        function outer(n) {{ return middle(n) + middle(n + 1); }}
                        function* worker() {{
                            with ({{}}) {{}}
                            let sum = 0;
                            for (let n = 0; n < 800; n++) sum += outer(n);
                            yield sum;
                            return sum;
                        }}
                        'ready';
                        "#
                    );
                    assert_eq!(value(&mut engine, &setup), "ready");
                    let expected: usize = (0..800)
                        .map(|n| {
                            [n, n + 1].into_iter()
                                .map(|k| k + owner + format!("engine-{owner}-{k}").len())
                                .sum::<usize>()
                        })
                        .sum();
                    for _ in 0..12 {
                        assert_eq!(value(&mut engine, "globalThis.run = worker(); run.next().value;"), expected.to_string());
                        assert_eq!(value(&mut engine, "run.next().value;"), expected.to_string());
                    }
                })
                .unwrap()
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

fn value(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).expect("parse") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
