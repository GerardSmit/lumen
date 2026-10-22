//! HTML rendering defaults for elements with a `popover` attribute.

use alloc::format;

use super::{
    color_scheme, context_length_value, static_length_context, typed_declarations, ContainerUnitContext, Style,
    UsedColorScheme, Value,
};

pub(super) fn apply(style: &mut Style, scheme: UsedColorScheme) {
    if let Some(color) = color_scheme::system_color("canvastext", scheme) {
        Value::Color(color).apply(style);
    }
    if let Some(color) = color_scheme::system_color("canvas", scheme) {
        Value::Background(color).apply(style);
    }
    let padding = format!("{}px", style.font_size * 0.25);
    let declarations: [(&str, &str); 12] = [
        ("position", "fixed"),
        ("top", "0px"),
        ("right", "0px"),
        ("bottom", "0px"),
        ("left", "0px"),
        ("margin", "auto"),
        ("width", "fit-content"),
        ("height", "fit-content"),
        ("border-width", "3px"),
        ("border-style", "solid"),
        ("padding", &padding),
        ("overflow", "auto"),
    ];
    for (name, raw) in declarations {
        if let Ok(parsed) = typed_declarations(name, raw, 0) {
            for declaration in parsed {
                match declaration.value {
                    Value::ContextLength(slot, raw, nonnegative) => {
                        if let Some(value) = context_length_value(
                            slot,
                            &raw,
                            static_length_context(),
                            nonnegative,
                            ContainerUnitContext::default(),
                        ) {
                            value.apply(style);
                        }
                    }
                    value => value.apply(style),
                }
            }
        }
    }
}
