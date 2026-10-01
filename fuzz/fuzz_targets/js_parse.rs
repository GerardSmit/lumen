#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen::bench_api::{parse_module, parse_script};

fuzz_target!(|data: &str| {
    let src = data.to_owned();
    lumen_fuzz::on_worker(move || {
        let _ = parse_script(&src, false);
        let _ = parse_script(&src, true);
        let _ = parse_module(&src);
    });
});
