#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if input.len() <= 32 * 1024 {
        let _ = lumen_html::css::parse(input);
    }
});
