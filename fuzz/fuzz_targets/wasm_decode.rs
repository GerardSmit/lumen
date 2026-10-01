#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let bytes = data
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // validate + decode + instantiate with no imports; modules that import anything just reject.
    let src = format!(
        "try {{ const b = new Uint8Array([{bytes}]); WebAssembly.validate(b); \
         const m = new WebAssembly.Module(b); new WebAssembly.Instance(m, {{}}); }} catch (e) {{}}"
    );
    lumen_fuzz::eval_runtime(src);
});
