#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if input.len() > 32 * 1024 { return; }
    let _ = lumen_html_image::render_html(input, 320, 240, 1.0);
});
