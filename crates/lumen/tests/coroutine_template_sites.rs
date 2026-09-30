use lumen::{Completion, Engine};

fn value(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).expect("parse") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn coroutine_lazy_templates_do_not_alias_driver_templates() {
    let mut engine = Engine::new();
    assert_eq!(
        value(
            &mut engine,
            r#"
        globalThis.driverTemplate = String.raw`driver shell text`;
        function* worker() {
            with ({}) {}
            function query() { return String.raw`SELECT session WHERE key = ?`; }
            return query();
        }
        worker().next().value;
    "#
        ),
        "SELECT session WHERE key = ?"
    );
}

#[test]
fn pooled_coroutine_template_sites_are_distinct_and_stable() {
    let mut engine = Engine::new();
    assert_eq!(
        value(
            &mut engine,
            r#"
        const capture = strings => strings;
        const first = capture`driver`;
        function* worker() {
            with ({}) {}
            function query() { return capture`worker`; }
            const initial = query();
            yield initial;
            return query();
        }
        const runs = [];
        for (let n = 0; n < 16; n++) {
            const workerRun = worker();
            const start = workerRun.next().value;
            const end = workerRun.next().value;
            runs.push(start === end && start !== first && start[0] === 'worker');
        }
        runs.every(Boolean);
    "#
        ),
        "true"
    );
}
