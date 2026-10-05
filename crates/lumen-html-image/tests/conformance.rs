use lumen_html_image::{decode_png, encode_png, render_html_with_font, Rgba8Image};
use lumen_html_text::{FontFace, TEST_FONT_BYTES};
use std::sync::Arc;

#[test]
fn integral_boxes_and_overflow_match_chromium() {
    assert_matches_chromium(
        "geometry",
        include_str!("render/geometry.html"),
        include_bytes!("render/geometry.png"),
    );
}

#[test]
fn flex_rows_columns_gaps_grow_and_alignment_match_chromium() {
    assert_matches_chromium(
        "flex",
        include_str!("render/flex.html"),
        include_bytes!("render/flex.png"),
    );
}

#[test]
fn flex_wrapping_and_line_stretch_match_chromium() {
    assert_matches_chromium(
        "wrap",
        include_str!("render/wrap.html"),
        include_bytes!("render/wrap.png"),
    );
}

#[test]
fn min_max_widths_and_flex_freezing_match_chromium() {
    assert_matches_chromium(
        "width",
        include_str!("render/width.html"),
        include_bytes!("render/width.png"),
    );
}

fn assert_matches_chromium(name: &str, html: &str, reference: &[u8]) {
    let font = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
    let actual = render_html_with_font(html, 64, 64, 1.0, &font).unwrap();
    let expected = decode_png(reference).unwrap();
    assert_eq!(
        (actual.width, actual.height),
        (expected.width, expected.height)
    );
    let mut max_delta = 0u8;
    let mut differing_pixels = 0;
    let mut difference = Rgba8Image {
        width: actual.width,
        height: actual.height,
        pixels: vec![0; actual.pixels.len()],
    };
    for ((actual, expected), diff) in actual
        .pixels
        .chunks_exact(4)
        .zip(expected.pixels.chunks_exact(4))
        .zip(difference.pixels.chunks_exact_mut(4))
    {
        let changed = actual != expected;
        differing_pixels += usize::from(changed);
        for channel in 0..3 {
            diff[channel] = actual[channel].abs_diff(expected[channel]);
            max_delta = max_delta.max(diff[channel]);
        }
        max_delta = max_delta.max(actual[3].abs_diff(expected[3]));
        diff[3] = 255;
    }
    if differing_pixels != 0 {
        let path = std::env::temp_dir().join(format!("lumen-html-{name}-diff.png"));
        std::fs::write(&path, encode_png(&difference)).unwrap();
        panic!("Chromium {name}: {differing_pixels} differing pixels, max channel delta {max_delta}; diff {}", path.display());
    }
}
