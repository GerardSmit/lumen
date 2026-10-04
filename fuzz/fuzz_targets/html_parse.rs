#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if input.len() > 32 * 1024 { return; }
    if let Ok(mut document) = lumen_html::html::parse(input, 256) {
        let root = document.root();
        if let Ok(serialized) = lumen_html::html::inner_html(&document, root) {
            let _ = lumen_html::html::parse(&serialized, 256);
        }
        // Fragment insertion uses the same mutation path and tree builder.
        let _ = lumen_html::html::parse_fragment(&mut document, input);
    }
});
