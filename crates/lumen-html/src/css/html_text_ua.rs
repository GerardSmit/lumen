//! HTML rendering defaults for links and edit/strike phrasing elements.

use super::{color_scheme, typed_declarations, Style, UsedColorScheme, Value};

/// `:any-link { color: LinkText; text-decoration: underline; cursor: pointer }`.
pub(super) fn link(style: &mut Style, scheme: UsedColorScheme) {
    if let Some(color) = color_scheme::system_color("linktext", scheme) {
        Value::Color(color).apply(style);
    }
    apply(style, &[("text-decoration-line", "underline"), ("cursor", "pointer")]);
}

/// `hr { color: gray; border-style: inset; border-width: 1px; overflow: hidden }`;
/// the block margins and automatic inline margins come with the flow defaults.
pub(super) fn rule(style: &mut Style) {
    apply(style, &[("color", "gray"), ("border-style", "inset"), ("border-width", "1px"), ("overflow", "hidden")]);
    style.border_color = style.color;
    let current = style.current_color_source();
    style.set_source_color(10, Some(current));
}

pub(super) fn decoration(style: &mut Style, line: &str) {
    apply(style, &[("text-decoration-line", line)]);
}

fn apply(style: &mut Style, declarations: &[(&str, &str)]) {
    for (name, raw) in declarations {
        if let Ok(parsed) = typed_declarations(name, raw, 0) {
            for declaration in parsed {
                declaration.value.apply(style);
            }
        }
    }
}
