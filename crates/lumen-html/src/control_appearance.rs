//! One HTML widget presentation policy for layout, pseudo eligibility and ink.
//! The state is operation-local; semantics and live values remain in shared DOM.
use crate::{
    css::{Appearance, Display, Style, WritingMode},
    forms, Document, Namespace, NodeId, NodeKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Widget {
    Button,
    Text,
    Textarea,
    Checkbox,
    Radio,
    Range,
    Color,
    File,
    Select,
    Meter,
    Progress,
}
#[derive(Clone, Copy)]
pub(crate) struct Presentation {
    pub widget: Widget,
    pub native: bool,
    pub replaced: bool,
    pub base_select: bool,
    pub list_box: bool,
    pub dropdown_button: bool,
}
pub(crate) fn presentation(
    document: &Document,
    node: NodeId,
    style: &Style,
) -> Option<Presentation> {
    let NodeKind::Element {
        name,
        namespace: Namespace::Html,
        ..
    } = document.kind(node).ok()?
    else {
        return None;
    };
    let widget = match crate::svg::local_name(name) {
        "button" => Widget::Button,
        "textarea" => Widget::Textarea,
        "select" => Widget::Select,
        "meter" => Widget::Meter,
        "progress" => Widget::Progress,
        "input" => match forms::input_type_state(document, node) {
            "hidden" | "image" => return None,
            "button" | "submit" | "reset" => Widget::Button,
            "checkbox" => Widget::Checkbox,
            "radio" => Widget::Radio,
            "range" => Widget::Range,
            "color" => Widget::Color,
            "file" => Widget::File,
            _ => Widget::Text,
        },
        _ => return None,
    };
    let list_box = widget == Widget::Select
        && (document.get_attribute_ns_ref(node, None, "multiple").ok().flatten().is_some()
            || forms::select_display_size(document, node).unwrap_or(1) > 1);
    let non_devolvable = matches!(widget, Widget::Checkbox | Widget::Radio | Widget::Range);
    let native = style.appearance != Appearance::None
        && (non_devolvable || !style.widget_devolved)
        && !(widget == Widget::Select && style.appearance == Appearance::MenulistButton);
    // Button contents remain real contents for every appearance. Primitive
    // checkbox/radio have no semantic internal contents and expose ordinary
    // children; editable text and range controls preserve their operation.
    let replaced = (widget != Widget::Button || crate::svg::local_name(name) == "input")
        && !(matches!(widget, Widget::Checkbox | Widget::Radio) && !native);
    Some(Presentation {
        widget,
        native,
        replaced,
        list_box,
        // HTML drop-down boxes retain their button in the devolved state;
        // primitive appearance:none and base appearance are separate states.
        dropdown_button: widget == Widget::Select && !list_box
            && !matches!(style.appearance, Appearance::None | Appearance::Base | Appearance::BaseSelect)
            && (native || style.widget_devolved || style.appearance == Appearance::MenulistButton),
        base_select: widget == Widget::Select
            && matches!(style.appearance, Appearance::Base | Appearance::BaseSelect),
    })
}

/// Width reserved for the arrow of a native drop-down box.
pub(crate) const MENULIST_ARROW_WIDTH: f32 = 12.0;
pub(crate) const MENULIST_BLOCK_PADDING: f32 = 1.0;
pub(crate) const MENULIST_INLINE_PADDING: f32 = 4.0;

/// Physical `[top, right, bottom, left]` padding of the anonymous inner block
/// Chromium puts around the label of a drop-down box, on top of the element's
/// own padding. Primitive `appearance: none` and base selects have none, and a
/// size-contained axis has none.
pub(crate) fn menulist_internal_padding(
    document: &Document,
    node: NodeId,
    style: &Style,
) -> Option<[f32; 4]> {
    let [top, right, bottom, left] = menulist_inner_padding(document, node, style)?;
    // The inner block is part of the contents that size containment replaces.
    let (horizontal, vertical) = (!style.contains_size_axis(true), !style.contains_size_axis(false));
    Some([
        if vertical { top } else { 0.0 },
        if horizontal { right } else { 0.0 },
        if vertical { bottom } else { 0.0 },
        if horizontal { left } else { 0.0 },
    ])
}

/// The inner block padding of a drop-down box whatever its containment: its
/// label is still laid out inside that block.
pub(crate) fn menulist_inner_padding(
    document: &Document,
    node: NodeId,
    style: &Style,
) -> Option<[f32; 4]> {
    if matches!(style.display, Display::None | Display::Contents) {
        return None;
    }
    let control = presentation(document, node, style)?;
    if control.widget != Widget::Select || !control.dropdown_button {
        return None;
    }
    let (block, inline) = (MENULIST_BLOCK_PADDING, MENULIST_INLINE_PADDING);
    Some(if style.writing_mode == WritingMode::HorizontalTb {
        [block, inline, block, inline]
    } else {
        [inline, block, inline, block]
    })
}
