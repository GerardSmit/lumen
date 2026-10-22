//! HTML rendering defaults for links and edit/strike phrasing elements.

use super::{color_scheme, typed_declarations, Style, UsedColorScheme, Value};

/// `:any-link { color: LinkText; text-decoration: underline; cursor: pointer }`.
pub(super) fn link(style: &mut Style, scheme: UsedColorScheme) {
    if let Some(color) = color_scheme::system_color("linktext", scheme) {
        Value::Color(color).apply(style);
    }
    apply(style, &[("text-decoration-line", "underline"), ("cursor", "pointer")]);
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
