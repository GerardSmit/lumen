#![cfg(feature = "native")]

use lumen_aot::{build::Spec, native};

fn target() -> lumen_common::target::TargetSpec {
    let mut target = lumen::target::host();
    // Host IR/linking test target, not a claim that this ABI is published.
    target.native_fp = 1;
    target
}

#[test]
fn native_script_publishes_global_functions_and_updates_global_vars() {
    let root = std::env::temp_dir().join(format!("lumen-native-globals-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.js"), r#"
        function request() { return count; }
        var count = 1;
        count = 7;
        count++;
        count = count + 4;
        let hidden = 99;
    "#).unwrap();
    let mut engine = lumen::Engine::new();
    let target = lumen::target::host();
    let spec = Spec { scripts: vec!["app.js".into()], ..Spec::default() };
    let build = native::compile(&root, &spec, &target).unwrap();
    let bytes = build.encode(&target).unwrap();
    engine.load_native_value_owned(bytes.into(), None, &[], true).unwrap();
    let global = engine.global_this();
    let function = engine.ctx().get_member(&global, "request").unwrap_or_else(|_| panic!("request lookup failed"));
    let result = engine.call_function(&function, global.clone(), &[]).unwrap_or_else(|_| panic!("request call failed"));
    assert!(matches!(result, lumen::embed::Value::Num(12.0)));
    assert!(matches!(engine.ctx().get_member(&global, "count").unwrap_or_else(|_| panic!("count lookup failed")), lumen::embed::Value::Num(12.0)));
    assert!(matches!(engine.ctx().get_member(&global, "hidden").unwrap_or_else(|_| panic!("hidden lookup failed")), lumen::embed::Value::Undefined));
    std::fs::remove_file(root.join("app.js")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn native_output_is_reproducible_and_has_only_native_sections() {
    let root = std::env::temp_dir().join(format!("lumen-native-producer-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.js"), "function answer() { return 42; } answer();").unwrap();
    let spec = Spec { scripts: vec!["app.js".into()], ..Spec::default() };
    let target = target();
    let first = native::compile(&root, &spec, &target).unwrap();
    let second = native::compile(&root, &spec, &target).unwrap();
    let bytes = first.encode(&target).unwrap();
    assert_eq!(bytes, second.encode(&target).unwrap());
    let container = lumen_common::aot::NativeContainer::parse(&bytes).unwrap();
    assert_eq!(container.functions.len(), 2);
    assert!(container.sections.iter().any(|section| section.kind == lumen_common::aot::SEC_NATIVE_LINES));
    let (stripped, map) = first.encode_stripped(&target).unwrap();
    let sidecar = lumen_common::aot::sidecar::Sidecar::decode(&map).unwrap();
    sidecar.validate_for_blob(&stripped).unwrap();
    assert_eq!(sidecar.lookup(0, 0), Some(("app.js", 1, 0)));
    assert!(sidecar.validate_for_blob(&bytes).is_err());
    assert!(first.payload.starts_with(b"LUMJSN03"));
    lumen::native_aot::validate_payload(&first.payload, first.image.functions.len()).unwrap();
    assert!(lumen::native_aot::validate_payload(&first.payload[..first.payload.len() - 1], first.image.functions.len()).is_err());
    assert!(lumen::native_aot::validate_payload(&first.payload, first.image.functions.len() + 1).is_err());
    assert!(!first.payload.windows(root.to_string_lossy().len()).any(|window|
        window == root.to_string_lossy().as_bytes()));
    let snapshot = lumen::heap_snapshot::Snapshot::capture(&[], &[], |_, _| None, |_| None, |_| None).unwrap();
    let mut initialized = second;
    initialized.snapshot_entry = Some(1);
    assert!(initialized.attach_snapshot(&snapshot).unwrap_err().contains("reachable native closure environment"));
    std::fs::remove_file(root.join("app.js")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn native_class_metadata_round_trips_without_ast() {
    let root = std::env::temp_dir().join(format!("lumen-native-class-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.js"), "class Example { field = 12; method() { return this.field; } } new Example().method();").unwrap();
    let spec = Spec { scripts: vec!["app.js".into()], ..Spec::default() };
    let build = native::compile(&root, &spec, &target()).unwrap();
    assert_eq!(build.image.functions.len(), 3);
    lumen::native_aot::validate_payload(&build.payload, build.image.functions.len()).unwrap();
    std::fs::remove_file(root.join("app.js")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn initialized_snapshot_resolves_native_functions_and_rejects_entry_calls() {
    let root = std::env::temp_dir().join(format!("lumen-native-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = "globalThis.cfg = {answer: 42}; cfg.self = cfg; function main() { return cfg.answer; }";
    std::fs::write(root.join("app.js"), source).unwrap();
    let spec = Spec { scripts: vec!["app.js".into()], ..Spec::default() };
    let mut build = native::compile_with_options(&root, &spec, &target(), None, Some("main")).unwrap();
    assert_eq!(build.snapshot_entry, Some(1));
    let mut engine = lumen::Engine::new();
    let baseline = lumen::precompiled::NativeSnapshotBaseline::new(&engine);
    engine.eval(source, false).unwrap();
    let snapshot = build.snapshot_functions.capture(&engine, &baseline).unwrap();
    build.attach_snapshot(&snapshot).unwrap();
    lumen::native_aot::validate_payload(&build.payload, build.image.functions.len()).unwrap();
    for call in ["main();", "main?.();", "(main)();", "`${main()}`;"] {
        std::fs::write(root.join("app.js"), format!("{source} {call}")).unwrap();
        assert!(native::compile_with_options(&root, &spec, &target(), None, Some("main")).err().unwrap().contains("must not directly call"));
    }
    std::fs::remove_file(root.join("app.js")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn module_top_level_await_builds_a_native_resume_table() {
    let root = std::env::temp_dir().join(format!("lumen-native-tla-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.mjs"), "export const answer = await Promise.resolve(42);").unwrap();
    let spec = Spec { entry: Some("app.mjs".into()), ..Spec::default() };
    let build = native::compile(&root, &spec, &target()).unwrap();
    lumen::native_aot::validate_payload(&build.payload, build.image.functions.len()).unwrap();
    std::fs::remove_file(root.join("app.mjs")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn member_trimming_keeps_transitive_calls_exports_and_requested_symbols() {
    let root = std::env::temp_dir().join(format!("lumen-native-trim-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.mjs"), "function dead() { return 0; } function leaf() { return 42; } function live() { return leaf(); } export function api() { return 1; } live();").unwrap();
    let mut spec = Spec { entry: Some("app.mjs".into()), trim_modules: true, trim_level: Some("members".into()), ..Spec::default() };
    let build = native::compile(&root, &spec, &target()).unwrap();
    assert_eq!(build.image.functions.len(), 4);
    assert!(build.trim_report.iter().any(|line| line == "trimmed: app.mjs.dead"));
    lumen::native_aot::validate_payload(&build.payload, build.image.functions.len()).unwrap();
    spec.keep.push("d*".into());
    let kept = native::compile(&root, &spec, &target()).unwrap();
    assert_eq!(kept.image.functions.len(), 5);
    assert!(!kept.trim_report.iter().any(|line| line.starts_with("trimmed:")));
    std::fs::remove_file(root.join("app.mjs")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn native_module_bindings_round_trip() {
    let root = std::env::temp_dir().join(format!("lumen-native-module-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.mjs"), "import { answer } from './value.mjs'; export default answer;").unwrap();
    std::fs::write(root.join("value.mjs"), "export const answer = 42;").unwrap();
    let spec = Spec { entry: Some("main.mjs".into()), walk: true, ..Spec::default() };
    let build = native::compile(&root, &spec, &target()).unwrap();
    assert_eq!(build.image.functions.len(), 2);
    lumen::native_aot::validate_payload(&build.payload, build.image.functions.len()).unwrap();
    std::fs::remove_file(root.join("main.mjs")).unwrap();
    std::fs::remove_file(root.join("value.mjs")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn native_requires_a_closed_source_graph() {
    let root = std::env::temp_dir().join(format!("lumen-native-import-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.js"), "function load(name) { return import(name); }").unwrap();
    let spec = Spec { scripts: vec!["app.js".into()], ..Spec::default() };
    let error = native::compile(&root, &spec, &target()).err().unwrap();
    assert!(error.contains("computed") && error.contains("app.js"), "{error}");
    std::fs::remove_file(root.join("app.js")).unwrap();
    std::fs::remove_dir(root).unwrap();
}
