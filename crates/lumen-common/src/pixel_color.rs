//! Bulk conversion of straight-alpha RGBA8 samples between the RGB working spaces
//! canvases can use. Coordinates come from the CSS Color conversions in `color`.
use crate::color::{linear_to_srgb, srgb_to_linear, Color, ColorSpace};

/// Whether `space` is one of the RGB working spaces this module converts.
pub fn is_rgb_working_space(space: ColorSpace) -> bool {
    matches!(
        space,
        ColorSpace::Srgb | ColorSpace::SrgbLinear | ColorSpace::DisplayP3 | ColorSpace::DisplayP3Linear
    )
}

fn is_transfer_encoded(space: ColorSpace) -> bool {
    matches!(space, ColorSpace::Srgb | ColorSpace::DisplayP3)
}

fn linear_counterpart(space: ColorSpace) -> ColorSpace {
    match space {
        ColorSpace::Srgb | ColorSpace::SrgbLinear => ColorSpace::SrgbLinear,
        _ => ColorSpace::DisplayP3Linear,
    }
}

/// Linear-light matrix taking `from` primaries to `to` primaries, row major.
fn gamut_matrix(from: ColorSpace, to: ColorSpace) -> [[f32; 3]; 3] {
    let (from, to) = (linear_counterpart(from), linear_counterpart(to));
    if from == to {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let mut matrix = [[0.0; 3]; 3];
    for column in 0..3 {
        let mut basis = [0.0; 3];
        basis[column] = 1.0;
        let converted = Color::new(from, basis, 1.0, 0).to(to).components;
        for row in 0..3 {
            matrix[row][column] = converted[row];
        }
    }
    matrix
}

/// Rounds a 0..1 coordinate to 8 bits; NaN becomes 0 and out-of-range values clamp.
pub fn byte(value: f32) -> u8 {
    if value.is_nan() {
        0
    } else {
        (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }
}

/// Converts one unclamped RGB sample between working spaces in float precision, keeping
/// out-of-range values. Other spaces leave the sample as is.
pub fn convert_rgb_f32(rgb: [f32; 3], from: ColorSpace, to: ColorSpace) -> [f32; 3] {
    if from == to || !is_rgb_working_space(from) || !is_rgb_working_space(to) {
        return rgb;
    }
    let linear = if is_transfer_encoded(from) { rgb.map(srgb_to_linear) } else { rgb };
    let m = gamut_matrix(from, to);
    let mapped = [
        m[0][0] * linear[0] + m[0][1] * linear[1] + m[0][2] * linear[2],
        m[1][0] * linear[0] + m[1][1] * linear[1] + m[1][2] * linear[2],
        m[2][0] * linear[0] + m[2][1] * linear[1] + m[2][2] * linear[2],
    ];
    if is_transfer_encoded(to) { mapped.map(linear_to_srgb) } else { mapped }
}

/// Converts `pixels` (straight RGBA, 8 bits per channel) in place. Alpha is untouched and
/// out-of-gamut results are clipped to the destination range. Other spaces leave the data as is.
pub fn convert_rgba8(pixels: &mut [u8], from: ColorSpace, to: ColorSpace) {
    if from == to || !is_rgb_working_space(from) || !is_rgb_working_space(to) {
        return;
    }
    let matrix = gamut_matrix(from, to);
    let decode_curve = is_transfer_encoded(from);
    let encode_curve = is_transfer_encoded(to);
    let mut decode = [0.0f32; 256];
    for (index, slot) in decode.iter_mut().enumerate() {
        let value = index as f32 / 255.0;
        *slot = if decode_curve { srgb_to_linear(value) } else { value };
    }
    let same_gamut = linear_counterpart(from) == linear_counterpart(to);
    let mut last_input = [0u8; 3];
    let mut last_output = [0u8; 3];
    let mut have_last = false;
    for pixel in pixels.chunks_exact_mut(4) {
        let input = [pixel[0], pixel[1], pixel[2]];
        if !(have_last && input == last_input) {
            let linear = [decode[input[0] as usize], decode[input[1] as usize], decode[input[2] as usize]];
            let mapped = if same_gamut {
                linear
            } else {
                [
                    matrix[0][0] * linear[0] + matrix[0][1] * linear[1] + matrix[0][2] * linear[2],
                    matrix[1][0] * linear[0] + matrix[1][1] * linear[1] + matrix[1][2] * linear[2],
                    matrix[2][0] * linear[0] + matrix[2][1] * linear[1] + matrix[2][2] * linear[2],
                ]
            };
            last_output = mapped.map(|value| {
                let clipped = value.clamp(0.0, 1.0);
                byte(if encode_curve { linear_to_srgb(clipped) } else { clipped })
            });
            last_input = input;
            have_last = true;
        }
        pixel[..3].copy_from_slice(&last_output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_red_is_inside_display_p3() {
        let mut pixel = [255, 0, 0, 255];
        convert_rgba8(&mut pixel, ColorSpace::Srgb, ColorSpace::DisplayP3);
        assert!((i32::from(pixel[0]) - 234).abs() <= 1);
        assert!((i32::from(pixel[1]) - 51).abs() <= 1);
        assert!((i32::from(pixel[2]) - 35).abs() <= 1);
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn display_p3_red_clips_to_srgb_red() {
        let mut pixel = [255, 0, 0, 128];
        convert_rgba8(&mut pixel, ColorSpace::DisplayP3, ColorSpace::Srgb);
        assert_eq!(pixel, [255, 0, 0, 128]);
    }

    #[test]
    fn linear_transfer_roundtrips_mid_gray() {
        let mut pixel = [188, 188, 188, 255];
        convert_rgba8(&mut pixel, ColorSpace::Srgb, ColorSpace::SrgbLinear);
        assert!((i32::from(pixel[0]) - 128).abs() <= 1);
        convert_rgba8(&mut pixel, ColorSpace::SrgbLinear, ColorSpace::Srgb);
        assert!((i32::from(pixel[0]) - 188).abs() <= 2);
    }

    #[test]
    fn float_conversion_keeps_out_of_gamut_values() {
        let p3 = [5.0 / 255.0, 250.0 / 255.0, 128.0 / 255.0];
        let srgb = convert_rgb_f32(p3, ColorSpace::DisplayP3, ColorSpace::Srgb);
        assert!(srgb[0] < 0.0);
        let back = convert_rgb_f32(srgb, ColorSpace::Srgb, ColorSpace::DisplayP3);
        for channel in 0..3 {
            assert!((back[channel] - p3[channel]).abs() < 1e-4);
        }
    }
}
