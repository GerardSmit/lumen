#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let source = input.to_owned();
    lumen_fuzz::on_worker(move || {
        for ts in [false, true] {
            let options = lumen::JsxOptions::default();
            let _ = lumen::bench_api::parse_module_jsx(&source, ts, &options);
            let _ = lumen::transpile_jsx(&source, ts, &options);
        }
    });
});
