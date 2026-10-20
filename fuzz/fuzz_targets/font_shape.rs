#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

fuzz_target!(|data: &[u8]| {
    if data.len() > 512 * 1024 { return; }
    let Ok(face) = lumen_html_text::FontFace::new(Arc::from(data)) else { return };
    for text in ["Hello, world 123", "\u{05e9}\u{05dc}\u{05d5}\u{05dd} abc \u{0627}\u{0644}\u{0639}", "fi ffl \u{0e01}\u{0e33} \u{0915}\u{094d}\u{0937}"] {
        if let Ok(run) = face.shape(text, 16.0) {
            for glyph in run.glyphs.iter().take(16) {
                let _ = face.rasterize(glyph.id, 16.0);
            }
        }
        let _ = face.measure(text, 24.0);
    }
});
