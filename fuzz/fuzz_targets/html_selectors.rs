#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if input.len() > 4096 { return; }
    let document = lumen_html::html::parse("<main id='root'><p class='a'>one</p><p lang='en'><b>two</b></p></main>", 64).unwrap();
    let root = document.root();
    let _ = lumen_html::selector::query_selector_all(&document, root, input);
    let _ = lumen_html::selector::matches(&document, root, input);
    let _ = lumen_html::selector::closest(&document, root, input);
});
