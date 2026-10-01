use crate::{Completion, Engine, Value, collect_disposed_realms};

#[test]
fn disposed_realms_reclaim_closure_scope_and_prototype_cycles() {
    std::thread::spawn(|| {
        let baseline = crate::value::live_objects();
        for _ in 0..3 {
            let mut engine = Engine::new();
            assert!(matches!(engine.eval(r#"
                for (let i = 0; i < 512; i++) {
                    const node = { payload: new Array(64).fill(i) };
                    node.self = node;
                    node.read = () => node.payload[0];
                    globalThis["slot" + i] = node;
                }
                1;
            "#, false).unwrap(), Completion::Value(value) if value == "1"));
            drop(engine);
            collect_disposed_realms();
            assert_eq!(
                crate::value::live_objects(),
                baseline,
                "completed workers must not leave live cycles for heap teardown"
            );
        }
    })
    .join()
    .unwrap();
}

#[test]
fn disposed_realm_collection_preserves_external_handles() {
    std::thread::spawn(|| {
        let baseline = crate::value::live_objects();
        let mut engine = Engine::new();
        let body = crate::parser::parse_script(
            "const node = { marker: 7 }; node.self = node; node;",
            false,
        )
        .unwrap();
        let value = engine
            .interp
            .run_program_parsed(&body)
            .unwrap_or_else(|_| panic!("object script threw"));
        drop(body);
        drop(engine);
        collect_disposed_realms();
        let Value::Obj(object) = &value else {
            panic!("expected object");
        };
        assert!(matches!(
            object.borrow().props.get("marker").unwrap().value(),
            Value::Num(7.0)
        ));
        assert!(
            matches!(object.borrow().props.get("self").unwrap().value(), Value::Obj(other)
            if crate::value::Gc::as_ptr(&other) == crate::value::Gc::as_ptr(&object))
        );
        drop(value);
        collect_disposed_realms();
        assert_eq!(crate::value::live_objects(), baseline);
    })
    .join()
    .unwrap();
}

#[test]
fn function_constructor_loop_keeps_the_lazy_registry_bounded() {
    std::thread::spawn(|| {
        let mut engine = Engine::new();
        let before = crate::value::lazy_function_registry_len();
        assert!(matches!(engine.eval(r#"
            let s = 0;
            for (let i = 0; i < 30000; i++) s += new Function("a", "return a + " + i)(1);
            s === 30000 + 29999 * 30000 / 2;
        "#, false).unwrap(), Completion::Value(value) if value == "true"));
        let after = crate::value::lazy_function_registry_len();
        assert!(after < before + 16384, "registry grew from {before} to {after}");
    })
    .join()
    .unwrap();
}

#[test]
fn throwaway_computed_key_objects_keep_the_shape_table_bounded() {
    std::thread::spawn(|| {
        let mut engine = Engine::new();
        let before = crate::value::shape_table_census().shapes;
        // Distinct first keys (one parent's transitions) and distinct second keys under 1000
        // parents (reclamation proper).
        assert!(matches!(engine.eval(r#"
            let s = 0;
            for (let i = 0; i < 60000; i++) {
                const k = "k" + i;
                const o = { [k]: 1 };
                const p = { ["p" + (i % 1000)]: 1, ["q" + i]: 2 };
                s += o[k] + p["q" + i];
            }
            s;
        "#, false).unwrap(), Completion::Value(value) if value == "180000"));
        let after = crate::value::shape_table_census().shapes;
        assert!(after < before + 16384, "shape table grew from {before} to {after}");
    })
    .join()
    .unwrap();
}

#[test]
fn creation_caches_survive_their_recorded_shape_being_reclaimed() {
    std::thread::spawn(|| {
        let mut engine = Engine::new();
        assert!(matches!(engine.eval(r#"
            function add(o, v) { o.fresh = v; return o; }
            let s = 0;
            for (let r = 0; r < 5; r++) {
                for (let i = 0; i < 200; i++) s += add({}, i).fresh;
                for (let i = 0; i < 20000; i++) {
                    const o = { ["z" + r + "_" + (i % 1000)]: 1, ["w" + i]: 2 };
                }
            }
            const o = add({}, 7);
            Object.keys(o).join() + ":" + s + ":" + o.fresh;
        "#, false).unwrap(), Completion::Value(value) if value == "fresh:99500:7"));
    })
    .join()
    .unwrap();
}

const CYCLES: &str = r#"
    for (let i = 0; i < 512; i++) {
        const node = { payload: new Array(64).fill(i) };
        node.self = node;
        node.read = () => node.payload[0];
        globalThis["slot" + i] = node;
    }
    class A { m() { return this; } }
    globalThis.inst = new A();
    1;
"#;

#[test]
fn dropping_the_last_engine_reclaims_its_cycles() {
    std::thread::spawn(|| {
        let baseline = crate::value::live_objects();
        for _ in 0..3 {
            let mut engine = Engine::new();
            assert!(matches!(engine.eval(CYCLES, false).unwrap(),
                Completion::Value(value) if value == "1"));
            drop(engine);
            assert_eq!(
                crate::value::live_objects(),
                baseline,
                "dropping an engine must not leave its realm's cycles live"
            );
        }
    })
    .join()
    .unwrap();
}

#[test]
fn engine_drop_defers_collection_while_another_engine_lives() {
    std::thread::spawn(|| {
        let baseline = crate::value::live_objects();
        let mut outer = Engine::new();
        assert!(matches!(outer.eval("globalThis.keep = { a: [1, 2, 3] }; keep.self = keep; 0", false).unwrap(),
            Completion::Value(value) if value == "0"));
        let mut inner = Engine::new();
        assert!(matches!(inner.eval(CYCLES, false).unwrap(),
            Completion::Value(value) if value == "1"));
        drop(inner);
        assert!(matches!(outer.eval("keep.self.a.length + keep.a[2]", false).unwrap(),
            Completion::Value(value) if value == "6"));
        drop(outer);
        assert_eq!(crate::value::live_objects(), baseline);
    })
    .join()
    .unwrap();
}
