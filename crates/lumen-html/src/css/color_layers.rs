//! `color-layers()` resolution: source-over compositing with a W3C Compositing
//! blend mode in sRGB. The first listed color is the top layer.
use lumen_common::color::{Color, ColorSpace};

const MODES: [&str; 16] = [
    "normal", "multiply", "screen", "overlay", "darken", "lighten", "color-dodge", "color-burn", "hard-light", "soft-light", "difference",
    "exclusion", "hue", "saturation", "color", "luminosity",
];

pub(super) fn blend_mode(keyword: &str) -> Option<&'static str> {
    MODES.iter().copied().find(|mode| *mode == keyword)
}

fn separable(mode: &str, backdrop: f32, source: f32) -> f32 {
    match mode {
        "multiply" => backdrop * source,
        "screen" => backdrop + source - backdrop * source,
        "overlay" => separable("hard-light", source, backdrop),
        "darken" => backdrop.min(source),
        "lighten" => backdrop.max(source),
        "color-dodge" => {
            if backdrop == 0.0 {
                0.0
            } else if source >= 1.0 {
                1.0
            } else {
                (backdrop / (1.0 - source)).min(1.0)
            }
        }
        "color-burn" => {
            if backdrop >= 1.0 {
                1.0
            } else if source <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - backdrop) / source).min(1.0)
            }
        }
        "hard-light" => {
            if source <= 0.5 {
                backdrop * 2.0 * source
            } else {
                separable("screen", backdrop, 2.0 * source - 1.0)
            }
        }
        "soft-light" => {
            if source <= 0.5 {
                backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
            } else {
                let d = if backdrop <= 0.25 { ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop } else { libm::sqrtf(backdrop) };
                backdrop + (2.0 * source - 1.0) * (d - backdrop)
            }
        }
        "difference" => (backdrop - source).abs(),
        "exclusion" => backdrop + source - 2.0 * backdrop * source,
        _ => source,
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut out = c;
    if n < 0.0 {
        for v in &mut out {
            *v = l + (*v - l) * l / (l - n);
        }
    }
    if x > 1.0 {
        for v in &mut out {
            *v = l + (*v - l) * (1.0 - l) / (x - l);
        }
    }
    out
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip([c[0] + d, c[1] + d, c[2] + d])
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let mut order = [0usize, 1, 2];
    order.sort_by(|a, b| c[*a].partial_cmp(&c[*b]).unwrap_or(core::cmp::Ordering::Equal));
    let [min, mid, max] = order;
    let mut out = [0.0; 3];
    if c[max] > c[min] {
        out[mid] = (c[mid] - c[min]) * s / (c[max] - c[min]);
        out[max] = s;
    }
    out
}

fn blend(mode: &str, backdrop: [f32; 3], source: [f32; 3]) -> [f32; 3] {
    match mode {
        "hue" => set_lum(set_sat(source, sat(backdrop)), lum(backdrop)),
        "saturation" => set_lum(set_sat(backdrop, sat(source)), lum(backdrop)),
        "color" => set_lum(source, lum(backdrop)),
        "luminosity" => set_lum(backdrop, lum(source)),
        _ => core::array::from_fn(|i| separable(mode, backdrop[i], source[i])),
    }
}

/// Composite `layers` (top first) into one sRGB color.
pub(super) fn composite(mode: &str, layers: &[Color]) -> Option<Color> {
    let mut iter = layers.iter().rev();
    let bottom = iter.next()?;
    let mut backdrop = bottom.to(ColorSpace::Srgb);
    let mut backdrop_alpha = if bottom.missing & 8 != 0 { 0.0 } else { backdrop.alpha };
    for layer in iter {
        let source = layer.to(ColorSpace::Srgb);
        let source_alpha = if layer.missing & 8 != 0 { 0.0 } else { source.alpha };
        let result_alpha = source_alpha + backdrop_alpha * (1.0 - source_alpha);
        let blended = blend(mode, backdrop.components, source.components);
        let mut components = [0.0; 3];
        if result_alpha > 0.0 {
            let ratio = source_alpha / result_alpha;
            for i in 0..3 {
                let mixed = (1.0 - backdrop_alpha) * source.components[i] + backdrop_alpha * blended[i];
                components[i] = (1.0 - ratio) * backdrop.components[i] + ratio * mixed;
            }
        }
        backdrop = Color::new(ColorSpace::Srgb, components, result_alpha, 0);
        backdrop_alpha = result_alpha;
    }
    Some(backdrop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_top_layer_wins() {
        let top = Color::rgba8([255, 0, 0, 255]);
        let bottom = Color::rgba8([0, 0, 255, 255]);
        assert_eq!(composite("normal", &[top, bottom]).unwrap().to_rgba8(), [255, 0, 0, 255]);
    }

    #[test]
    fn multiply_blends_with_the_backdrop() {
        let top = Color::rgba8([255, 255, 0, 255]);
        let bottom = Color::rgba8([0, 255, 255, 255]);
        assert_eq!(composite("multiply", &[top, bottom]).unwrap().to_rgba8(), [0, 255, 0, 255]);
    }
}
