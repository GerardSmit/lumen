#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let (flags, pattern) = match data.split_once('\n') {
        Some((f, p)) if f.len() <= 8 => (f, p),
        _ => ("", data),
    };
    let flags = lumen_fuzz::js_string_literal(flags);
    let pattern = lumen_fuzz::js_string_literal(pattern);
    lumen_fuzz::eval(
        format!("try {{ new RegExp({pattern}, {flags}).test(\"aAbB 0\\n\"); }} catch (e) {{}}"),
        false,
    );
});
