#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Parse and register without executing arbitrary JS; corruption must be a clean error.
    if data.len() > 1024 * 1024 {
        return;
    }
    let bytes: std::sync::Arc<[u8]> = data.into();
    lumen_fuzz::on_worker(move || {
        let _ = lumen_common::aot::Container::parse(&bytes);
        let _ = lumen_common::aot::NativeContainer::parse(&bytes);
        let _ = lumen_common::target::TargetSpec::decode(&bytes);
        let blob = lumen::Precompiled::from_bytes(bytes);
        let _ = blob.validate();
        let mut engine = lumen::Engine::new();
        let _ = engine.register_precompiled(&blob);
        drop(engine);
        lumen::collect_disposed_realms();
    });
});
