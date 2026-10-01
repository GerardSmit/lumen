#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen::bench_api::{decode, encode, parse_script};

const SRC: &str = "function f(a, b) { return a + b * 2; }\nlet o = { x: [1, 2, 3], y: `t${f(1, 2)}` };\nf(o.x[0], 4);";

fuzz_target!(|data: &[u8]| {
    let bytes = data.to_vec();
    lumen_fuzz::on_worker(move || {
        let _ = decode(&bytes, SRC);
        let _ = decode(&bytes, "");
        // Corrupt a valid blob so the fuzzer gets past the header checks.
        if let Ok(body) = parse_script(SRC, false) {
            let mut blob = encode(&body, SRC);
            for (i, b) in bytes.iter().enumerate() {
                let at = (i * 7 + 8) % blob.len().max(1);
                if let Some(slot) = blob.get_mut(at) {
                    *slot ^= *b;
                }
            }
            let _ = decode(&blob, SRC);
        }
    });
});
