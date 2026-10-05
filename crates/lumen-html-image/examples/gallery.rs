//! Deterministic visual smoke page with the embedded proportional conformance face.
use lumen_html::layout::ImageState;
use lumen_html::paint::ImageData;
use lumen_html_image::{encode_png, render_html_with_images};
use lumen_html_text::{FontFace, TEST_FONT_BYTES};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args().nth(1).ok_or("usage: gallery OUTPUT.png")?;
    let font = FontFace::new(Arc::from(TEST_FONT_BYTES))?;
    let tile = Arc::new(ImageData {
        width: 2,
        height: 2,
        pixels: vec![
            255, 160, 32, 255, 32, 160, 255, 255, 32, 160, 255, 255, 255, 160, 32, 255,
        ],
    });
    let images = |source: &str| {
        if source == "tile" {
            ImageState::Ready(tile.clone())
        } else {
            ImageState::Failed
        }
    };
    let html = "<style>html{background:#203040;color:white}body{padding:12px}div.card{background:#e8eef6;border:2px solid #5599cc;border-radius:12px;padding:12px;width:220px;color:#203040;font-size:22px}p{font-size:16px}div.clip{overflow:hidden;width:40px;height:20px;border:2px solid white}div.wide{width:80px;height:40px;background:#ff8020}</style><div class='card'>Lumen HTML<br>rounded border</div><p>Proportional  text &amp; whitespace</p><img src='tile' width='40' height='40'><div class='clip'><div class='wide'></div></div>";
    let image = render_html_with_images(html, 320, 240, 2.0, &font, &images)
        .map_err(|error| format!("render failed: {error:?}"))?;
    std::fs::write(output, encode_png(&image))?;
    Ok(())
}
