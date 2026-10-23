use super::*;

/// A parsed canvas `filter` value: the canonical serialization plus the
/// ordered operations. `none` yields an empty operation list.
#[derive(Clone, Debug)]
pub struct CanvasFilter {
    pub text: String,
    pub operations: Arc<[lumen_common::filter::FilterOperation]>,
}

pub fn parse_canvas_filter(raw: &str, current_color: Rgba, font_px: f32) -> Option<CanvasFilter> {
    let raw = raw.trim();
    if decoded_css_keyword(raw, "none") {
        return Some(CanvasFilter { text: "none".into(), operations: Arc::from([]) });
    }
    let font = if font_px.is_finite() && font_px > 0.0 { font_px } else { 10.0 };
    let mut context = static_length_context();
    context.font = font;
    context.ex = font * 0.5;
    context.ch = font * 0.5;
    context.units = FontLengthUnits { own: FontUnitBases::syntax(font), root: FontUnitBases::syntax(16.0) };
    let query = ContainerUnitContext::no_container(context.viewport);
    let list = filter_source_list(raw, &legacy_source_color(current_color), context, query)?;
    if list.filters.iter().any(|filter| matches!(filter, lumen_common::filter::FilterOperation::Url(_))) {
        return None;
    }
    let text = serialize_color_filter_declaration(raw)?;
    Some(CanvasFilter { text, operations: list.filters })
}
