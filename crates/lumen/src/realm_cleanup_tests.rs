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
            assert!(
                crate::value::live_objects() > baseline,
                "disposed cycles should require the post-realm collection"
            );
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
