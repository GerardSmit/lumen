//! Raster images whose encoded form carries an ICC profile.
use crate::Rgba8Image;

/// Decodes `bytes` and converts the pixels from their embedded profile into sRGB or Display P3.
/// `None` without a usable RGB profile.
pub(crate) fn decode_with_profile(bytes: &[u8], display_p3: bool) -> Option<Rgba8Image> {
    let profile = crate::raster_image_icc_profile(bytes).ok()??;
    let source = moxcms::ColorProfile::new_from_slice(&profile).ok()?;
    let mut image = crate::decode_raster_image_bounded(bytes, true, crate::MAX_IMAGE_BYTES)
        .ok()
        .filter(Rgba8Image::is_valid)?;
    let destination = if display_p3 { moxcms::ColorProfile::new_display_p3() } else { moxcms::ColorProfile::new_srgb() };
    crate::raster_image_convert_between_profiles(&mut image, &source, &destination).ok()?;
    Some(image)
}

/// Converts decoded pixels from the profile embedded in `bytes` into sRGB, leaving them
/// unchanged when there is no usable RGB profile.
pub(crate) fn color_manage_to_srgb(bytes: &[u8], image: &mut Rgba8Image) {
    let Ok(Some(profile)) = crate::raster_image_icc_profile(bytes) else { return };
    let Ok(source) = moxcms::ColorProfile::new_from_slice(&profile) else { return };
    let mut converted = image.clone();
    if crate::raster_image_convert_to_srgb_profile(&mut converted, &source).is_ok() {
        *image = converted;
    }
}
