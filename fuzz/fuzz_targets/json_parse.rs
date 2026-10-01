#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let lit = lumen_fuzz::js_string_literal(data);
    lumen_fuzz::eval(
        format!("try {{ JSON.parse({lit}); }} catch (e) {{}}"),
        false,
    );
});
