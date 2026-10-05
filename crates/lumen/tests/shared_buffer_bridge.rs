//! Native shared-memory capabilities preserve identity across actual OS-thread realms.
#![cfg(feature = "embed")]
use lumen::{Completion, Engine};

fn passed(engine: &mut Engine, source: &str) {
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => assert_eq!(value, "passed"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn native_capability_aliases_across_threads_and_wakes_waiters() {
    let mut owner = Engine::new();
    let buffer = owner
        .eval_value("new SharedArrayBuffer(8)")
        .unwrap()
        .ok()
        .expect("evaluation must succeed");
    let handle = owner
        .ctx()
        .export_shared_array_buffer(&buffer)
        .ok()
        .expect("export must succeed")
        .unwrap();
    let global = owner.global_this();
    owner
        .ctx()
        .set_member(&global, "shared", buffer)
        .ok()
        .expect("install owner buffer");
    let worker = std::thread::spawn(move || {
        let mut realm = Engine::new();
        let imported = realm.ctx().import_shared_array_buffer(&handle);
        let global = realm.global_this();
        realm
            .ctx()
            .set_member(&global, "shared", imported)
            .ok()
            .expect("install worker buffer");
        passed(
            &mut realm,
            r#"
            if (!(shared instanceof SharedArrayBuffer) || shared.byteLength !== 8) throw Error('brand');
            const state = new Int32Array(shared);
            for (let turn = 1; turn <= 200; turn++) {
                Atomics.store(state, 0, turn);
                Atomics.notify(state, 0);
                while (Atomics.load(state, 1) !== turn) {
                    if (Atomics.wait(state, 1, turn - 1, 1000) === 'timed-out') throw Error('owner stalled');
                }
            }
            'passed'
        "#,
        );
    });
    passed(
        &mut owner,
        r#"
        const state = new Int32Array(shared);
        for (let turn = 1; turn <= 200; turn++) {
            while (Atomics.load(state, 0) !== turn) {
                if (Atomics.wait(state, 0, turn - 1, 1000) === 'timed-out') throw Error('worker stalled');
            }
            Atomics.store(state, 1, turn);
            Atomics.notify(state, 1);
        }
        if (Atomics.load(state, 0) !== 200) throw Error('shared identity lost');
        'passed'
    "#,
    );
    worker.join().unwrap();
}

#[test]
fn forged_handles_are_not_exported_and_growable_sharing_is_refused() {
    let mut realm = Engine::new();
    for source in ["({__sab_id: 1})", "new ArrayBuffer(4)", "new Int32Array(1)"] {
        let value = realm
            .eval_value(source)
            .unwrap()
            .ok()
            .expect("evaluation must succeed");
        assert!(realm
            .ctx()
            .export_shared_array_buffer(&value)
            .ok()
            .expect("plain values are not errors")
            .is_none());
    }
    let growable = realm
        .eval_value("new SharedArrayBuffer(4, {maxByteLength: 8})")
        .unwrap()
        .ok()
        .expect("growable construction");
    assert!(realm.ctx().export_shared_array_buffer(&growable).is_err());
}

#[test]
fn wait_expected_value_wraps_to_the_element_width() {
    let mut realm = Engine::new();
    passed(
        &mut realm,
        r#"
        const words = new Int32Array(new SharedArrayBuffer(4));
        if (Atomics.wait(words, 0, 4294967296, 0) !== 'timed-out') throw Error('Int32 expected wrapping');
        const wide = new BigInt64Array(new SharedArrayBuffer(8));
        if (Atomics.wait(wide, 0, 18446744073709551616n, 0) !== 'timed-out') throw Error('BigInt expected wrapping');
        if (Atomics.waitAsync(words, 0, 4294967296, 0).value !== 'timed-out') throw Error('async expected wrapping');
        'passed'
    "#,
    );
}
