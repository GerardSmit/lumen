#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 { return; }
    const LIMIT: usize = 4 * 1024 * 1024;
    let _ = lumen_html_image::decode_png(data);
    let _ = lumen_html_image::decode_raster_image_with_limit(data, LIMIT);
    let _ = lumen_html_image::decode_image_with_limit(data, LIMIT);
    if let Ok(true) = lumen_html_image::raster_image_has_icc_profile(data) {
        if let (Ok(Some(profile)), Ok(mut image)) = (
            lumen_html_image::raster_image_icc_profile(data),
            lumen_html_image::decode_raster_image_with_limit(data, LIMIT),
        ) {
            let _ = lumen_html_image::raster_image_convert_to_srgb(&mut image, &profile);
        }
    }
});
