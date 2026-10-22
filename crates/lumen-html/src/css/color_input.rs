//! Value sanitization for `<input type=color>`: parses any absolute CSS color
//! and serializes it for the control's `colorspace` and `alpha` attributes.
use super::typed_colors::{parse_typed_color, TypedColor};
use super::*;
use alloc::format;
use lumen_common::color::{Color, ColorSpace};

fn number(value: f32) -> String {
    let text = format!("{:.6}", value as f64);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-0" { "0".to_owned() } else { text.to_owned() }
}

fn parse(value: &str) -> Color {
    let black = Color::rgba8([0, 0, 0, 255]);
    match parse_typed_color(value) {
        Some(TypedColor::Absolute { color, .. }) => {
            if matches!(color.space, ColorSpace::Srgb | ColorSpace::Hsl | ColorSpace::Hwb) {
                Color::rgba8(color.to_rgba8())
            } else {
                color
            }
        }
        Some(TypedColor::Keyword(name)) => color_scheme::system_color(&name, UsedColorScheme::Light)
            .map_or(black, |c| Color::rgba8([c.r, c.g, c.b, c.a])),
        None => black,
    }
}

pub fn sanitize_input_color(value: &str, display_p3: bool, alpha: bool) -> String {
    let color = parse(value);
    if !display_p3 {
        let [r, g, b, a] = color.to_rgba8();
        if !alpha {
            return format!("#{r:02x}{g:02x}{b:02x}");
        }
        let channel = |v: u8| number(v as f32 / 255.0);
        let mut out = format!("color(srgb {} {} {}", channel(r), channel(g), channel(b));
        if a != 255 {
            out.push_str(" / ");
            out.push_str(&channel(a));
        }
        out.push(')');
        return out;
    }
    let converted = color.to(ColorSpace::DisplayP3);
    let mut out = String::from("color(display-p3");
    for (index, component) in converted.components.iter().enumerate() {
        out.push(' ');
        out.push_str(&number(if converted.missing & (1 << index) != 0 { 0.0 } else { *component }));
    }
    if alpha && converted.missing & 8 == 0 && converted.alpha < 1.0 {
        out.push_str(" / ");
        out.push_str(&number(converted.alpha.max(0.0)));
    }
    out.push(')');
    out
}
