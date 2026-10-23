//! User-agent defaults of `::marker` that must not be inherited from the list item.

use super::{extra_props, value_props, LengthPercentage, Style, TextTransform, Value};
use alloc::sync::Arc;

pub(super) fn apply(style: &mut Style) {
    Value::ExtraRaw(
        extra_props::VALUE_BASE + value_props::const_index("font-variant-numeric"),
        Arc::from("tabular-nums"),
    )
    .apply(style);
    style.text_transform = TextTransform::None;
    style.text_indent = LengthPercentage { pixels: 0.0, fraction: 0.0 };
    style.text_indent_mode = 0;
}
