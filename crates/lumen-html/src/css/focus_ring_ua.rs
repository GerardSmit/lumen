//! The user-agent focus ring of elements matching `:focus-visible`.

use super::{OutlineStyle, Rgba, Style};

pub(super) fn apply(style: &mut Style) {
    style.outline.style = OutlineStyle::Auto;
    style.outline.width = 1.0;
    style.outline.color = Some(Rgba { r: 16, g: 16, b: 16, a: 255 });
}
