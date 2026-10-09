//! Computed CSS serialization from shared typed state. No declaration parsing.
use super::declaration_block::serialize_box_sides;
use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct ComputedValueContext<'a> {
    /// CSSOM resolved values, distinct from computed values used by animation timing.
    pub resolved: bool,
    pub border_box: Option<Rect>,
    /// Exact SVG used reference; CSS layout aliases derive from the border box.
    pub transform_reference_box:Option<Rect>,
    /// Owner-qualified used map source from the same completed geometry epoch.
    /// Raw computed source remains available for new scroll/rendering samples.
    pub transform_timeline_source: Option<&'a str>,
    pub percentage_basis: Option<(f32, Option<f32>)>,
    /// Sparse used margin values recorded when layout resolved `auto` margins
    /// or adjusted a resolved side for its formatting constraint.
    pub used_margins: Option<[f32; 4]>,
    pub replaced_element: bool,
    /// Actual box formatting controls whether resolved dimensions apply.
    /// This carrier never changes serialized computed display.
    pub used_display: Option<Display>,
    pub primary_font_metric: Option<f32>,
    /// HTML widget used-line-height projection; canonical computed state is unchanged.
    pub used_line_height: Option<f32>,
    pub grid_columns: Option<&'a [f32]>,
    pub grid_rows: Option<&'a [f32]>,
    pub grid_column_names: Option<&'a [GridNamedLine]>,
    pub grid_row_names: Option<&'a [GridNamedLine]>,
}

pub fn needs_layout(name: &str) -> bool {
    matches!(
        name,
        "width"
            | "height"
            | "inline-size"
            | "block-size"
            | "min-width"
            | "max-width"
            | "min-height"
            | "max-height"
            | "min-inline-size"
            | "max-inline-size"
            | "min-block-size"
            | "max-block-size"
            | "transform"
            | "transform-origin"
            | "grid-template-columns"
            | "grid-template-rows"
            | "grid-template"
            | "grid"
            | "grid-lanes"
    ) || name.starts_with("margin")
        || name.starts_with("padding")
        || name.starts_with("inset")
        || matches!(name, "top" | "right" | "bottom" | "left")
}

pub(crate) fn number(value: f32) -> String {
    if value == 0.0 {
        String::from("0")
    } else {
        value.to_string()
    }
}
pub(crate) fn px(value: f32) -> String {
    alloc::format!("{}px", number(value))
}
pub(crate) fn color(value: Rgba) -> String {
    if value.a == 255 {
        return alloc::format!("rgb({}, {}, {})", value.r, value.g, value.b);
    }
    let alpha = value.a as f32 / 255.0;
    let hundredths = libm::roundf(alpha * 100.0) / 100.0;
    let alpha = if libm::roundf(hundredths * 255.0) == value.a as f32 {
        hundredths
    } else {
        libm::roundf(alpha * 1000.0) / 1000.0
    };
    alloc::format!(
        "rgba({}, {}, {}, {})",
        value.r,
        value.g,
        value.b,
        number(alpha)
    )
}
pub(crate) fn percentage(pixels: f32, percent: f32) -> String {
    if percent == 0.0 {
        px(pixels)
    } else if pixels == 0.0 {
        alloc::format!("{}%", number(percent))
    } else {
        alloc::format!(
            "calc({}% {} {}px)",
            number(percent),
            if pixels < 0.0 { "-" } else { "+" },
            number(pixels.abs())
        )
    }
}
pub(crate) fn lp(value: LengthPercentage) -> String {
    percentage(value.pixels, value.fraction * 100.0)
}
fn transform_reference(style:&Style,context:ComputedValueContext<'_>)->Option<Rect> {
    if let Some(reference)=context.transform_reference_box {return Some(reference);}
    let border=context.border_box?;
    let resolved=context.percentage_basis
        .filter(|_|!style.relative_lengths.is_empty() || !style.relative_expressions.is_empty())
        .map(|(width,height)|style.resolve_percentages(width,height));
    Some(resolved.as_ref().unwrap_or(style).css_transform_reference_box(border))
}

fn pair(a: String, b: String) -> String {
    if a == b {
        a
    } else {
        alloc::format!("{a} {b}")
    }
}
pub(crate) fn four(values: [String; 4]) -> String {
    serialize_box_sides(values.each_ref().map(String::as_str))
}
fn overflow(value: Overflow) -> &'static str {
    match value {
        Overflow::Visible => "visible",
        Overflow::Hidden => "hidden",
        Overflow::Clip => "clip",
        Overflow::Auto => "auto",
        Overflow::Scroll => "scroll",
    }
}
fn intrinsic(value: IntrinsicSizing) -> &'static str {
    value.as_str()
}
fn side(name: &str) -> Option<usize> {
    if name.contains("-top") {
        Some(0)
    } else if name.contains("-right") {
        Some(1)
    } else if name.contains("-bottom") {
        Some(2)
    } else if name.contains("-left") {
        Some(3)
    } else {
        None
    }
}
fn logical_side(style: &Style, name: &str) -> Option<usize> {
    let index = if name.contains("-inline-start") {
        0
    } else if name.contains("-inline-end") {
        1
    } else if name.contains("-block-start") {
        2
    } else if name.contains("-block-end") {
        3
    } else {
        return None;
    };
    Some(style.logical_sides()[index])
}
fn style_color(style:&Style,slot:usize,fallback:Rgba)->String {
    let slot=if matches!(slot,79) && style.column_rule_color.is_none() || slot==187 && style.outline.color.is_none() || slot==203 && style.text_decoration_color.is_none() {1}else {slot};
    if style.source_color(slot).is_some() {
        let source=style.resolved_source_color(slot,fallback);
        if let Some(expression)=source.expression.as_ref().filter(|expression|slot!=1 && !super::decoded_css_keyword(&expression.raw,"currentcolor")) {
            return expression.raw.to_string();
        }
        if let Some(value)=super::color_values::serialize(source.value,source.color_function) {return value;}
    }
    color(fallback)
}

fn style_color_for_phase(style:&Style,slot:usize,fallback:Rgba,resolved:bool)->String {
    if resolved && style.source_color(slot).is_some() {
        let source=style.resolved_source_color(slot,fallback);
        if let Some(value)=super::color_values::serialize(source.value,source.color_function){return value;}
    }
    style_color(style,slot,fallback)
}

fn relative(style: &Style, slot: usize) -> Option<String> {
    style
        .relative_lengths
        .iter()
        .find(|(key, _)| *key == slot)
        .map(|(_, value)| percentage(value.pixels, value.percent))
        .or_else(|| {
            style
                .relative_expressions
                .iter()
                .find(|value| value.slot == slot)
                .map(|value| value.raw.to_string())
        })
}
fn margin(style: &Style, used: &Style, edge: usize, context: ComputedValueContext<'_>) -> String {
    if let Some(values) = context.used_margins {
        return px(values[edge]);
    }
    if context.percentage_basis.is_none() {
        if let Some(value) = relative(style, 39 + edge) {
            return value;
        }
    }
    if used.margin_auto[edge] {
        "auto".into()
    } else {
        px(used.margin_sides[edge])
    }
}
fn padding(style: &Style, used: &Style, edge: usize, context: ComputedValueContext<'_>) -> String {
    if context.percentage_basis.is_none() {
        if let Some(value) = relative(style, 43 + edge) {
            return value;
        }
    }
    px(used.padding_sides()[edge])
}
fn offset(style: &Style, used: &Style, edge: usize, context: ComputedValueContext<'_>) -> String {
    let has_box =
        context.border_box.is_some() && !matches!(style.display, Display::None | Display::Contents);
    if !has_box || style.position == Position::Static {
        if let Some(value) = relative(style, 35 + edge) {
            return value;
        }
        return [style.top, style.right, style.bottom, style.left][edge]
            .map_or_else(|| "auto".into(), px);
    }
    if style.position == Position::Relative {
        return px(used.relative_insets()[edge]);
    }
    if context.percentage_basis.is_none() {
        if let Some(value) = relative(style, 35 + edge) {
            return value;
        }
    }
    [used.top, used.right, used.bottom, used.left][edge].map_or_else(|| "auto".into(), px)
}
fn size(
    style: &Style,
    used: &Style,
    horizontal: bool,
    context: ComputedValueContext<'_>,
) -> String {
    if let Some(rect) = context.border_box.filter(|_| {
        !matches!(style.display, Display::None | Display::Contents)
            && (context.used_display.unwrap_or(style.display) != Display::Inline || context.replaced_element)
    }) {
        let mut value = if horizontal { rect.width } else { rect.height };
        if style.box_sizing == BoxSizing::ContentBox {
            let edges = if horizontal { [1, 3] } else { [0, 2] };
            let borders = used.used_border_widths();
            let padding = used.padding_sides();
            // Subtract the complete box offset once, matching layout's
            // padding-then-border construction. Sequential side subtraction
            // introduces cancellation residue for an exactly zero content box.
            value -= padding[edges[0]] + padding[edges[1]] + borders[edges[0]] + borders[edges[1]];
        }
        return px(value.max(0.0));
    }
    if let Some(value) = relative(style, if horizontal { 3 } else { 4 }) {
        return value;
    }
    if let Some(value) = style.intrinsic_size_keywords(horizontal)[0] { return intrinsic(value).into(); }
    (if horizontal { used.width } else { used.height }).map_or_else(|| "auto".into(), px)
}
fn font_adjust(style: &Style, metric: Option<f32>) -> String {
    let Some(adjust) = style.font.size_adjust else {
        return "none".into();
    };
    let value = match adjust.value {
        FontSizeAdjustValue::Number(value) => number(value),
        FontSizeAdjustValue::FromFont => {
            let Some(metric) = metric.filter(|value| value.is_finite() && *value >= 0.0) else {
                return "none".into();
            };
            number(metric)
        },
    };
    if adjust.metric == FontMetric::ExHeight {
        value
    } else {
        alloc::format!(
            "{} {value}",
            match adjust.metric {
                FontMetric::ExHeight => "ex-height",
                FontMetric::CapHeight => "cap-height",
                FontMetric::ChWidth => "ch-width",
                FontMetric::IcWidth => "ic-width",
                FontMetric::IcHeight => "ic-height",
            }
        )
    }
}
pub(super) fn list_type(value: &ListStyleType) -> String {
    match value {
        ListStyleType::Disc => "disc".into(),
        ListStyleType::Circle => "circle".into(),
        ListStyleType::Square => "square".into(),
        ListStyleType::Decimal => "decimal".into(),
        ListStyleType::DecimalLeadingZero => "decimal-leading-zero".into(),
        ListStyleType::LowerRoman => "lower-roman".into(),
        ListStyleType::UpperRoman => "upper-roman".into(),
        ListStyleType::LowerAlpha => "lower-alpha".into(),
        ListStyleType::UpperAlpha => "upper-alpha".into(),
        ListStyleType::None => "none".into(),
        ListStyleType::String(value) => serialize_css_string(value),
        ListStyleType::Unsupported(value) => serialize_identifier(value),
    }
}
fn svg_paint(value: &SvgPaint, current: Rgba) -> Option<String> {
    Some(match value {
        SvgPaint::None => "none".into(),
        SvgPaint::Color(value) => color(*value),
        SvgPaint::CurrentColor => color(current),
        SvgPaint::Reference(url, fallback) => {
            let mut result = alloc::format!("url({})", serialize_css_string(url));
            if let Some(value) = fallback {
                result.push(' ');
                result.push_str(&color(*value));
            }
            result
        }
        SvgPaint::Unsupported(_) => return None,
    })
}

pub(crate) fn background_positions_css_value(values: Option<&[[LengthPercentage; 2]]>) -> Option<String> {
    values.map_or_else(|| Some("0% 0%".into()), |values|
        list(values,MAX_BACKGROUND_GEOMETRY_VALUES,", ",|value|Some(alloc::format!("{} {}",lp(value[0]),lp(value[1])))))
}
pub(crate) fn background_sizes_css_value(values: Option<&[BackgroundSize]>) -> Option<String> {
    values.map_or_else(||Some("auto".into()),|values|list(values,MAX_BACKGROUND_GEOMETRY_VALUES,", ",|value|Some(background_size(value))))
}
pub(crate) fn shadows_css_value(values: Option<&[BoxShadow]>) -> Option<String> {
    values.map_or_else(||Some("none".into()),|values|list(values,64,", ",|value|Some(alloc::format!(
        "{} {} {} {} {}{}",color(value.color),px(value.offset_x),px(value.offset_y),px(value.blur),px(value.spread),
        if value.inset {" inset"} else {""}))))
}

pub(crate) fn text_shadows_css_value(values: Option<&[BoxShadow]>) -> Option<String> {
    values.map_or_else(||Some("none".into()),|values|list(values,64,", ",|value|Some(alloc::format!(
        "{} {} {} {}",color(value.color),px(value.offset_x),px(value.offset_y),px(value.blur)))))
}

impl Style {

    pub fn computed_css_value(
        &self,
        name: &str,
        context: ComputedValueContext<'_>,
    ) -> Option<String> {
        if name.starts_with("--") {
            return self
                .custom_properties()
                .iter()
                .find(|(property, _)| property == name)
                .and_then(|(_, value)| value.clone());
        }
        // CSSOM resolved color special cases expose the used float color.
        // Computed/Typed OM serialization retains the inherited expression.
        let style_color=|style:&Style,slot:usize,fallback:Rgba| {
            style_color_for_phase(style,slot,fallback,context.resolved && matches!(slot,1|2|10|107..=110|119..=122|187))
        };
        let lowered = ascii_lower(name);
        let name = lowered.as_ref();
        if slots(name).is_empty() {
            return None;
        }
        let resolved = context
            .percentage_basis
            .filter(|_| !self.relative_lengths.is_empty() || !self.relative_expressions.is_empty())
            .map(|(width, height)| self.resolve_percentages(width, height));
        let used = resolved.as_ref().unwrap_or(self);
        let physical = side(name).or_else(|| logical_side(self, name));
        let result = match name {
            "display" if self.is_list_item() => match self.display {
                Display::Inline => "inline list-item",
                Display::InlineBlock => "inline flow-root list-item",
                Display::FlowRoot => "flow-root list-item",
                _ => "list-item",
            }.into(),
            "display" => match self.display {
                Display::Block => "block",
                Display::FlowRoot => "flow-root",
                Display::Inline => "inline",
                Display::InlineBlock => "inline-block",
                Display::Contents => "contents",
                Display::ListItem => "list-item",
                Display::Flex if self.display_inline => "inline-flex",
                Display::Flex => "flex",
                Display::Grid if self.grid_lanes && self.display_inline => "inline grid-lanes",
                Display::Grid if self.grid_lanes => "grid-lanes",
                Display::Grid if self.display_inline => "inline-grid",
                Display::Grid => "grid",
                Display::Table => "table",
                Display::InlineTable => "inline-table",
                Display::TableCaption => "table-caption",
                Display::TableRowGroup => "table-row-group",
                Display::TableHeaderGroup => "table-header-group",
                Display::TableFooterGroup => "table-footer-group",
                Display::TableColumn => "table-column",
                Display::TableColumnGroup => "table-column-group",
                Display::TableRow => "table-row",
                Display::TableCell => "table-cell",
                Display::None => "none",
            }
            .into(),
            "color" => style_color(self,1,self.color),
            "background-color" => style_color(self,2,self.background),
            "opacity" => number(self.opacity),
            "filter" => self.filters.as_ref().map_or_else(|| "none".into(), |filters| {
                use lumen_common::filter::ColorFilter as F;
                let mut shadow_index=0;
                filters.iter().map(|filter| {
                    let filter = match filter {
                        lumen_common::filter::FilterOperation::Color(filter) => *filter,
                        lumen_common::filter::FilterOperation::Blur(sigma) => return alloc::format!("blur({})",px(*sigma)),
                        lumen_common::filter::FilterOperation::Url(url)=>return super::serialize_url(&url.href),
                        lumen_common::filter::FilterOperation::Resource(resource)=>return super::serialize_url(&resource.url),
                        lumen_common::filter::FilterOperation::DropShadow(shadow) => {
                            let color=shadow.color;let offset=shadow.offset;let sigma=shadow.sigma;
                            let color=self.filter_shadow_color_value(shadow_index,color).unwrap_or_else(||"transparent".into());shadow_index+=1;
                            return alloc::format!("drop-shadow({color} {} {} {})",px(offset[0]),px(offset[1]),px(sigma));
                        }
                    };
                    let (name, value, suffix) = match filter {
                        F::Brightness(v) => ("brightness", v, ""), F::Contrast(v) => ("contrast", v, ""),
                        F::Grayscale(v) => ("grayscale", v, ""), F::HueRotate(v) => ("hue-rotate", v, "deg"),
                        F::Invert(v) => ("invert", v, ""), F::Opacity(v) => ("opacity", v, ""),
                        F::Saturate(v) => ("saturate", v, ""), F::Sepia(v) => ("sepia", v, ""),
                    };
                    alloc::format!("{name}({}{suffix})", number(value))
                }).collect::<Vec<_>>().join(" ")
            }),

            "width" => size(self, used, true, context),
            "height" => size(self, used, false, context),
            "inline-size" => size(
                self,
                used,
                self.writing_mode == WritingMode::HorizontalTb,
                context,
            ),
            "block-size" => size(
                self,
                used,
                self.writing_mode != WritingMode::HorizontalTb,
                context,
            ),
            "min-width" | "min-inline-size" | "min-block-size"
                if name == "min-width"
                    || (name == "min-inline-size")
                        == (self.writing_mode == WritingMode::HorizontalTb) =>
            {
                relative(self, 23).unwrap_or_else(|| {
                    if let Some(value) = self.min_width_intrinsic { intrinsic(value).into() }
                    else if self.min_width_auto {
                        "auto".into()
                    } else {
                        px(used.min_width)
                    }
                })
            }
            "max-width" | "max-inline-size" | "max-block-size"
                if name == "max-width"
                    || (name == "max-inline-size")
                        == (self.writing_mode == WritingMode::HorizontalTb) =>
            {
                relative(self, 24)
                    .unwrap_or_else(|| self.max_width_intrinsic.map_or_else(|| used.max_width.map_or_else(|| "none".into(), px), |value| intrinsic(value).into()))
            }
            "min-height" | "min-inline-size" | "min-block-size" => relative(self, 51)
                .unwrap_or_else(|| {
                    self.min_height_intrinsic.map_or_else(
                        || {
                            if self.min_height_auto {
                                "auto".into()
                            } else {
                                px(used.min_height)
                            }
                        },
                        |value| intrinsic(value).into(),
                    )
                }),
            "max-height" | "max-inline-size" | "max-block-size" => relative(self, 52)
                .unwrap_or_else(|| {
                    self.max_height_intrinsic.map_or_else(
                        || used.max_height.map_or_else(|| "none".into(), px),
                        |value| intrinsic(value).into(),
                    )
                }),
            "margin" => four(core::array::from_fn(|edge| {
                margin(self, used, edge, context)
            })),
            "padding" => four(core::array::from_fn(|edge| {
                padding(self, used, edge, context)
            })),
            "inset" => four(core::array::from_fn(|edge| {
                offset(self, used, edge, context)
            })),
            "margin-inline" | "margin-block" | "padding-inline" | "padding-block"
            | "inset-inline" | "inset-block" => {
                let logical = self.logical_sides();
                let start = if name.ends_with("inline") { 0 } else { 2 };
                let serialize = |edge| {
                    if name.starts_with("margin") {
                        margin(self, used, edge, context)
                    } else if name.starts_with("padding") {
                        padding(self, used, edge, context)
                    } else {
                        offset(self, used, edge, context)
                    }
                };
                pair(serialize(logical[start]), serialize(logical[start + 1]))
            }
            "top" | "right" | "bottom" | "left" => offset(
                self,
                used,
                match name {
                    "top" => 0,
                    "right" => 1,
                    "bottom" => 2,
                    _ => 3,
                },
                context,
            ),
            property if property.starts_with("margin-") => margin(self, used, physical?, context),
            property if property.starts_with("padding-") => padding(self, used, physical?, context),
            property if property.starts_with("inset-") => offset(self, used, physical?, context),
            "border-radius" => self.border_radius_css_value(),
            "border-top-left-radius" => self.border_radius_corner_css_value(0),
            "border-top-right-radius" => self.border_radius_corner_css_value(1),
            "border-bottom-right-radius" => self.border_radius_corner_css_value(2),
            "border-bottom-left-radius" => self.border_radius_corner_css_value(3),
            "border-start-start-radius" => {
                self.border_radius_corner_css_value(self.logical_corner_indices()[0])
            }
            "border-start-end-radius" => {
                self.border_radius_corner_css_value(self.logical_corner_indices()[1])
            }
            "border-end-start-radius" => {
                self.border_radius_corner_css_value(self.logical_corner_indices()[2])
            }
            "border-end-end-radius" => {
                self.border_radius_corner_css_value(self.logical_corner_indices()[3])
            }
            "outline-color" => style_color(self,187,self.outline.color.unwrap_or(self.color)),
            "outline-style" => self.outline.style.as_str().into(),
            "outline-width" => px(self.outline.used_width()),
            "outline-offset" => px(self.outline.offset),
            "outline" => declaration_block::serialize_outline_components(
                &style_color(self,187,self.outline.color.unwrap_or(self.color)),
                self.outline.style.as_str(),
                &px(self.outline.used_width()),
            ),
            "border-width" => four(used.used_border_widths().map(px)),
            "border-style" => four(self.border_styles().map(|value| value.as_str().into())),
            "border-color" => four(core::array::from_fn(|edge| {
                style_color(self,119+edge,self.border_color_sides[edge].unwrap_or(self.border_color))
            })),
            "border-inline-width" | "border-block-width" | "border-inline-style" | "border-block-style" | "border-inline-color" | "border-block-color" => {
                let edges = self.logical_sides();
                let start = if name.starts_with("border-inline-") {0} else {2};
                let component = |side: usize| {
                    if name.ends_with("-width") {px(used.used_border_widths()[side])}
                    else if name.ends_with("-style") {self.border_styles()[side].as_str().into()}
                    else {style_color(self,119+side,self.border_color_sides[side].unwrap_or(self.border_color))}
                };
                pair(component(edges[start]),component(edges[start+1]))
            }
            property if physical.is_some() && property.starts_with("border-") && property.ends_with("-width") => {
                px(used.used_border_widths()[physical?])
            }
            property if physical.is_some() && property.starts_with("border-") && property.ends_with("-style") => {
                self.border_styles()[physical?].as_str().into()
            }
            property if physical.is_some() && property.starts_with("border-") && property.ends_with("-color") => {
                style_color(self,119+physical?,self.border_color_sides[physical?].unwrap_or(self.border_color))
            }
            "border-top"
            | "border-right"
            | "border-bottom"
            | "border-left"
            | "border-inline-start"
            | "border-inline-end"
            | "border-block-start"
            | "border-block-end" => self.computed_border_side(physical?,context.resolved),
            "border" => {
                let values =
                    core::array::from_fn::<_, 4, _>(|edge| self.computed_border_side(edge,context.resolved));
                if values.iter().any(|value| value != &values[0]) {
                    return None;
                }
                values[0].clone()
            }
            "border-inline" | "border-block" => {
                let edges = self.logical_sides();
                let start = if name.ends_with("inline") { 0 } else { 2 };
                let a = self.computed_border_side(edges[start],context.resolved);
                (a == self.computed_border_side(edges[start + 1],context.resolved)).then_some(a)?
            }
            "overflow-x" => overflow(self.overflow_x).into(),
            "overflow-y" => overflow(self.overflow_y).into(),
            "overflow-inline" => overflow(if self.writing_mode==WritingMode::HorizontalTb {self.overflow_x}else{self.overflow_y}).into(),
            "overflow-block" => overflow(if self.writing_mode==WritingMode::HorizontalTb {self.overflow_y}else{self.overflow_x}).into(),
            "overflow" => pair(
                overflow(self.overflow_x).into(),
                overflow(self.overflow_y).into(),
            ),
            "font-size" => px(self.font_size),
            "container-type" => self.container_type.as_str().into(),
            "font-family" => self.font.families.as_ref().map_or_else(
                || "sans-serif".into(),
                |value| serialize_font_families(value),
            ),
            "font-weight" => self.font.weight.to_string(),
            "font-width" | "font-stretch" => { if self.font.unresolved_stretch.is_some() { return None; } alloc::format!("{}%", number(self.font.stretch)) },
            "font-style" => {
                if self.font.unresolved_style.is_some() { return None; }
                serialize_font_style(self.font.style)
            },
            "font-feature-settings" => {
                if self.font.feature_settings.as_ref().is_some_and(|settings| settings.resolved().is_none()) { return None; }
                serialize_font_feature_settings(self.font.feature_settings.as_deref())
            },
            "font-variant-alternates" => font_feature_values::serialize_alternates(self.font.alternates.as_deref()),
            "font-variant-ligatures" => serialize_font_ligatures(self.font.ligatures),
            "font-variant" => serialize_font_variant(self.font.caps,self.font.ligatures,self.font.alternates.as_deref())?,
            "font-variant-caps" => self.font.caps.as_str().into(),
            "font-synthesis-small-caps" => if self.font.synthesize_small_caps {
                "auto"
            } else {
                "none"
            }
            .into(),
            "font-size-adjust" => font_adjust(self, context.primary_font_metric),
            "line-height" => match self.line_height {
                LineHeight::Normal => "normal".into(),
                LineHeight::Number(value) => if context.resolved {
                    px(context.used_line_height.unwrap_or(value * self.font_size))
                } else { number(value) },
                LineHeight::Pixels(value) => px(if context.resolved {context.used_line_height.unwrap_or(value)} else {value}),
            },
            "font" => self.computed_font(context)?,
            "flex-direction" => match self.flex_direction {
                FlexDirection::Row => "row",
                FlexDirection::RowReverse => "row-reverse",
                FlexDirection::Column => "column",
                FlexDirection::ColumnReverse => "column-reverse",
            }
            .into(),
            "flex-wrap" => if self.flex_wrap_reverse {
                "wrap-reverse"
            } else if self.flex_wrap {
                "wrap"
            } else {
                "nowrap"
            }
            .into(),
            "flex-grow" => number(self.flex_grow),
            "flex-shrink" => number(self.flex_shrink),
            "flex-basis" => relative(self, 20).unwrap_or_else(|| {
                self.flex_basis_intrinsic.map_or_else(
                    || {
                        if self.flex_basis_content {
                            "content".into()
                        } else {
                            self.flex_basis.map_or_else(|| "auto".into(), px)
                        }
                    },
                    |value| intrinsic(value).into(),
                )
            }),
            "flex" => alloc::format!(
                "{} {} {}",
                number(self.flex_grow),
                number(self.flex_shrink),
                self.computed_css_value("flex-basis", context)?
            ),
            "flex-flow" => alloc::format!(
                "{} {}",
                self.computed_css_value("flex-direction", context)?,
                self.computed_css_value("flex-wrap", context)?
            ),
            "row-gap" => {
                if self.row_gap_normal {
                    "normal".into()
                } else {
                    percentage(self.gap, self.row_gap_fraction * 100.0)
                }
            }
            "column-gap" => self.column_gap.map_or_else(
                || "normal".into(),
                |value| percentage(value, self.column_gap_fraction * 100.0),
            ),
            "gap" => pair(
                self.computed_css_value("row-gap", context)?,
                self.computed_css_value("column-gap", context)?,
            ),
            "column-count" => self
                .column_count
                .map_or_else(|| "auto".into(), |value| value.to_string()),
            "break-before"|"break-after"|"break-inside"|"page-break-before"|"page-break-after"|"page-break-inside"=>{
                let slot=break_control::slot(name)?;break_control::serialize(name,self.break_controls()[slot-247]).unwrap_or("").into()
            },
            "column-span"=>self.column_span().serialize(),
            "box-decoration-break"=>self.box_decoration_break().serialize(),
            "orphans" => self.line_break_counts()[0].to_string(),
            "widows" => self.line_break_counts()[1].to_string(),
            "column-fill" => if self.column_fill_auto {
                "auto"
            } else {
                "balance"
            }
            .into(),
            "column-rule-width" => px(if self.column_rule_style.paints() {
                self.column_rule_width
            } else {
                0.0
            }),
            "column-rule-style" => self.column_rule_style.as_str().into(),
            "column-rule-color" => style_color(self,79,self.column_rule_color.unwrap_or(self.color)),
            "column-rule" => alloc::format!(
                "{} {} {}",
                self.computed_css_value("column-rule-width", context)?,
                self.computed_css_value("column-rule-style", context)?,
                self.computed_css_value("column-rule-color", context)?
            ),
            "box-sizing" => if self.box_sizing == BoxSizing::BorderBox {
                "border-box"
            } else {
                "content-box"
            }
            .into(),
            "column-width"=>self.column_width.serialize()?,
            "columns"=>super::columns::shorthand_value(&self.column_width.serialize()?,&self.column_count.map_or_else(||"auto".into(),|value|value.to_string())),
            "aspect-ratio" => self.aspect_ratio_components.or_else(|| self.aspect_ratio.map(|value| [value, 1.0])).map_or_else(
                || "auto".into(),
                |value| alloc::format!("{}{} / {}", if self.aspect_ratio_auto { "auto " } else { "" },
                    number(value[0]), number(value[1])),
            ),
            "position" => match self.position {
                Position::Static => "static",
                Position::Relative => "relative",
                Position::Sticky => "sticky",
                Position::Absolute => "absolute",
                Position::Fixed => "fixed",
            }
            .into(),
            "float" => match self.float {
                Float::None => "none",
                Float::Left => "left",
                Float::Right => "right",
            }
            .into(),
            "clear" => match self.clear {
                Clear::None => "none",
                Clear::Left => "left",
                Clear::Right => "right",
                Clear::Both => "both",
            }
            .into(),
            "z-index" => self
                .z_index
                .map_or_else(|| "auto".into(), |value| value.to_string()),
            "order" => self.order.to_string(),
            "text-orientation" => self.text_orientation.as_str().into(),
            "writing-mode" => match self.writing_mode {
                WritingMode::HorizontalTb => "horizontal-tb",
                WritingMode::VerticalRl => "vertical-rl",
                WritingMode::VerticalLr => "vertical-lr",
                WritingMode::SidewaysRl => "sideways-rl",
                WritingMode::SidewaysLr => "sideways-lr",
            }
            .into(),
            "unicode-bidi" => self.unicode_bidi.as_str().into(),
            "scroll-behavior" => self.scroll_behavior.as_str().into(),
            "text-overflow" => self.text_overflow_markers.as_ref().map_or_else(||self.text_overflow.as_str().into(), |value|value.serialize()),
            "direction" => if self.direction == Direction::Ltr {
                "ltr"
            } else {
                "rtl"
            }
            .into(),
            "visibility" => if self.visibility_visible {
                "visible"
            } else if self.visibility_collapsed {
                "collapse"
            } else {
                "hidden"
            }
            .into(),
            "pointer-events" => if self.pointer_events_auto {
                "auto"
            } else {
                "none"
            }
            .into(),
            "contain-intrinsic-width"|"contain-intrinsic-height"|"contain-intrinsic-inline-size"|"contain-intrinsic-block-size" => self.computed_intrinsic_override(slots(name)[0]),
            "contain-intrinsic-size" => {let a=self.computed_intrinsic_override(234);let b=self.computed_intrinsic_override(235);if a==b{a}else{alloc::format!("{a} {b}")}},
            "contain" => {
                let mut value = String::new();
                for (set, keyword) in [
                    (self.contain_size, "size"),
                    (self.contain_inline_size, "inline-size"),
                    (self.contain_layout, "layout"),
                    (self.contain_style, "style"),
                    (self.contain_paint, "paint"),
                ] {
                    if set {
                        if !value.is_empty() {
                            value.push(' ');
                        }
                        value.push_str(keyword);
                    }
                }
                if value.is_empty() {
                    "none".into()
                } else {
                    value
                }
            }
            "word-break" => self.word_break.as_str().into(),
            "overflow-wrap" | "word-wrap" => self.overflow_wrap.as_str().into(),
            "text-transform" => self.text_transform.as_str().into(),
            "white-space" => match self.white_space {
                WhiteSpace::Normal => "normal",
                WhiteSpace::NoWrap => "nowrap",
                WhiteSpace::Pre => "pre",
                WhiteSpace::PreWrap => "pre-wrap",
                WhiteSpace::PreLine => "pre-line",
                WhiteSpace::BreakSpaces => "break-spaces",
            }
            .into(),
            "text-align" => match self.text_align {
                TextAlign::Start => "start",
                TextAlign::End => "end",
                TextAlign::Left => "left",
                TextAlign::Right => "right",
                TextAlign::Center => "center",
                TextAlign::Justify => "justify",
                TextAlign::MatchParent => "match-parent",
                TextAlign::JustifyAll => "justify-all",
            }
            .into(),
            "text-indent" => lp(self.text_indent),
            "letter-spacing" => self.letter_spacing.map_or_else(|| "normal".into(), px),
            "word-spacing" => lp(self.word_spacing),
            "vertical-align" => match self.vertical_align {
                VerticalAlign::Baseline => "baseline".into(),
                VerticalAlign::Sub => "sub".into(),
                VerticalAlign::Super => "super".into(),
                VerticalAlign::TextTop => "text-top".into(),
                VerticalAlign::Middle => "middle".into(),
                VerticalAlign::BaselineMiddle => "-webkit-baseline-middle".into(),
                VerticalAlign::Top => "top".into(),
                VerticalAlign::Bottom => "bottom".into(),
                VerticalAlign::TextBottom => "text-bottom".into(),
                VerticalAlign::Length(value) => lp(value),
            },
            "text-decoration" => {
                let line = serialize_text_decoration_line(self.text_decoration);
                let mut result = String::new();
                if self.text_decoration != 0 { result.push_str(&line); }
                if self.text_decoration_thickness != DecorationLength::Auto {
                    if !result.is_empty() {result.push(' ');}result.push_str(&self.text_decoration_thickness.computed());
                }
                if self.text_decoration_style != TextDecorationStyle::Solid {
                    if !result.is_empty() { result.push(' '); } result.push_str(self.text_decoration_style.as_str());
                }
                let initial_color=self.source_color(203).and_then(|source|source.expression.clone())
                    .is_some_and(|expression|super::decoded_css_keyword(&expression.raw,"currentcolor"));
                if let Some(value) = self.text_decoration_color.filter(|_|!initial_color) {
                    if !result.is_empty() { result.push(' '); } result.push_str(&style_color(self,203,value));
                }
                if result.is_empty() { result.push_str("none"); }
                result
            },
            "text-decoration-style" => self.text_decoration_style.as_str().into(),
            "text-decoration-color" => style_color(self,203,self.text_decoration_color.unwrap_or(self.color)),
            "text-underline-position" => self.text_underline_position.serialize(),
            "text-decoration-thickness" => self.text_decoration_thickness.computed(),
            "text-underline-offset" => self.text_underline_offset.computed(),
            "text-decoration-line" => {
                let mut value = String::new();
                for (bit, keyword) in [(1, "underline"), (2, "overline"), (4, "line-through"), (8, "blink")] {
                    if self.text_decoration & bit != 0 {
                        if !value.is_empty() {
                            value.push(' ');
                        }
                        value.push_str(keyword);
                    }
                }
                if value.is_empty() {
                    "none".into()
                } else {
                    value
                }
            }
            "table-layout" => if self.table_fixed { "fixed" } else { "auto" }.into(),
            "border-collapse" => if self.border_collapse {
                "collapse"
            } else {
                "separate"
            }
            .into(),
            "border-spacing" => pair(px(self.border_spacing[0]), px(self.border_spacing[1])),
            "empty-cells" => if self.empty_cells_hide {
                "hide"
            } else {
                "show"
            }
            .into(),
            "caption-side" => if self.caption_bottom { "bottom" } else { "top" }.into(),
            "content" => serialize_generated_content(&self.generated_content()),
            "quotes" => serialize_quotes(self.quotes()),
            "counter-reset" => {
                serialize_counter_directives(CounterProperty::Reset, self.counter_reset.as_deref())?
            }
            "counter-increment" => serialize_counter_directives(
                CounterProperty::Increment,
                self.counter_increment.as_deref(),
            )?,
            "counter-set" => {
                serialize_counter_directives(CounterProperty::Set, self.counter_set.as_deref())?
            }
            "color-scheme" => self.color_scheme.as_deref().unwrap_or("normal").into(),
            "list-style-type" => list_type(&self.list_style_type),
            "list-style-position" => if self.list_style_position == ListStylePosition::Inside {
                "inside"
            } else {
                "outside"
            }
            .into(),
            "list-style-image" => self.computed_image_source_value(178).unwrap_or_else(||self.list_style_image.as_ref().map_or_else(
                || "none".into(),
                |raw| computed_marker_image(self).and_then(|value|image(&value,0)).unwrap_or_else(||alloc::format!("url({})", serialize_css_string(raw))),
            )),
            "list-style" => alloc::format!(
                "{} {} {}",
                self.computed_css_value("list-style-type", context)?,
                self.computed_css_value("list-style-position", context)?,
                self.computed_css_value("list-style-image", context)?
            ),
            "transition-property" => transition_controls::serialize_properties(self.transition_property()),
            "transition-timing-function" => transition_controls::serialize_timings(self.transition_timing_function()),
            "transition-behavior" => transition_controls::serialize_behaviors(self.transition_behavior()),
            "appearance" | "-webkit-appearance" => self.appearance.as_str().into(),
            "field-sizing" => self.field_sizing.as_str().into(),
            "view-transition-name" => match &self.view_transition_name {
                ViewTransitionName::None => "none".into(),
                ViewTransitionName::MatchElement => "match-element".into(),
                ViewTransitionName::Custom(name) => serialize_css_identifier(name),
            },
            "transition" => {
                let names=["transition-property","transition-duration","transition-timing-function","transition-delay","transition-behavior"];
                let values=names.iter().map(|name|self.computed_css_value(name,context)).collect::<Option<Vec<_>>>()?;
                transition_controls::shorthand(&values.iter().map(String::as_str).collect::<Vec<_>>())?
            }
            "transition-duration" => {
                serialize_transition_time_values(self.transition_duration.as_deref())
            }
            "transition-delay" => {
                serialize_transition_time_values(self.transition_delay.as_deref())
            }
            "fill" => if self.source_color(141).is_some() {style_color(self,141,self.color)}else {svg_paint(&self.svg_fill,self.color)?},
            "stroke" => if self.source_color(142).is_some() {style_color(self,142,self.color)}else {svg_paint(&self.svg_stroke,self.color)?},
            "stroke-width" => px(self.svg_stroke_width),
            "fill-opacity" => number(self.svg_fill_opacity),
            "stroke-opacity" => number(self.svg_stroke_opacity),
            "fill-rule" => if self.svg_fill_rule == SvgFillRule::EvenOdd {
                "evenodd"
            } else {
                "nonzero"
            }
            .into(),
            "clip-rule" => if self.svg_clip_rule == SvgFillRule::EvenOdd {
                "evenodd"
            } else {
                "nonzero"
            }
            .into(),
            "clip-path" => self.svg_clip_path.as_ref().map_or_else(
                || "none".into(),
                |url| alloc::format!("url({})", serialize_css_string(url)),
            ),
            "stop-color" => style_color(self,156,self.svg_stop_color),
            "stop-opacity" => number(self.svg_stop_opacity),
            "flood-color"=>style_color(self,252,self.svg_filter_properties().flood),
            "flood-opacity"=>number(self.svg_filter_properties().opacity),
            "color-interpolation-filters"=>self.svg_filter_properties().interpolation.css_keyword().into(),
            "x" | "y" | "rx" | "ry" | "cx" | "cy" | "r" => {
                let index = match name {
                    "x" => 0,
                    "y" => 1,
                    "rx" => 4,
                    "ry" => 5,
                    "cx" => 6,
                    "cy" => 7,
                    _ => 8,
                };
                self.svg_geometry[index].as_ref().map_or_else(
                    || {
                        if matches!(name, "rx" | "ry") {
                            "auto".into()
                        } else {
                            "0px".into()
                        }
                    },
                    |value| value.to_string(),
                )
            }
            "align-content" | "justify-content" | "align-items" | "justify-items"
            | "align-self" | "justify-self" | "place-items" | "place-self" | "place-content" => {
                self.alignment_css(name)?
            }
            "grid-lanes-direction" => serialize_lanes_direction(self.grid_lanes_direction),
            "grid-lanes-pack" => if self.grid_lanes_dense {
                "dense"
            } else {
                "normal"
            }
            .into(),
            "flow-tolerance" => match self.flow_tolerance {
                FlowTolerance::Normal => "normal".into(),
                FlowTolerance::Infinite => "infinite".into(),
                FlowTolerance::Length(value) => lp(value),
            },
            _ => self.computed_complex_css(name, context)?,
        };
        (result.len() <= MAX_CSS_BYTES).then_some(result)
    }
    fn computed_border_side(&self, edge: usize,resolved:bool) -> String {
        alloc::format!(
            "{} {} {}",
            px(self.used_border_widths()[edge]),
            self.border_styles()[edge].as_str(),
            style_color_for_phase(self,119+edge,self.border_color_sides[edge].unwrap_or(self.border_color),resolved)
        )
    }
    fn computed_font(&self, context: ComputedValueContext<'_>) -> Option<String> {
        if !self.font.ligatures.is_normal() || self.font.feature_settings.is_some() || self.font.size_adjust.is_some()
            || !matches!(
                self.font.caps,
                FontVariantCaps::Normal | FontVariantCaps::SmallCaps
            )
        {
            return None;
        }
        if self.font.unresolved_stretch.is_some() || self.font.unresolved_style.is_some() { return None; }
        let style = serialize_font_style(self.font.style);
        let mut result = String::new();
        if style != "normal" {
            result.push_str(&style);
            result.push(' ');
        }
        if self.font.caps != FontVariantCaps::Normal {
            result.push_str(self.font.caps.as_str());
            result.push(' ');
        }
        if self.font.weight != 400 {
            result.push_str(&self.font.weight.to_string());
            result.push(' ');
        }
        if self.font.stretch != 100.0 {
            result.push_str(font_stretch_percentage_keyword(self.font.stretch)?);
            result.push(' ');
        }
        result.push_str(&px(self.font_size));
        if self.line_height != LineHeight::Normal {
            result.push_str(" / ");
            result.push_str(
                &self
                    .computed_css_value("line-height", context)
                    .unwrap_or_default(),
            );
        }
        result.push(' ');
        result.push_str(
            &self
                .computed_css_value("font-family", context)
                .unwrap_or_default(),
        );
        Some(result)
    }
}

fn append(output: &mut String, value: &str) -> Option<()> {
    let size = output.len().checked_add(value.len())?;
    if size > MAX_CSS_BYTES {
        return None;
    }
    output.try_reserve(value.len()).ok()?;
    output.push_str(value);
    Some(())
}
fn list<T>(
    values: &[T],
    limit: usize,
    separator: &str,
    mut serialize: impl FnMut(&T) -> Option<String>,
) -> Option<String> {
    if values.len() > limit {
        return None;
    }
    let mut output = String::new();
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            append(&mut output, separator)?;
        }
        append(&mut output, &serialize(value)?)?;
    }
    Some(output)
}
fn background_box(value: BackgroundBox) -> String {
    declaration_block::serialize_background_box(value).into()
}
fn attachment(value: BackgroundAttachment) -> &'static str {
    match value {
        BackgroundAttachment::Scroll => "scroll",
        BackgroundAttachment::Fixed => "fixed",
        BackgroundAttachment::Local => "local",
    }
}
fn background_size(value: &BackgroundSize) -> String {
    match value.kind {
        BackgroundSizeKind::Cover => "cover".into(),
        BackgroundSizeKind::Contain => "contain".into(),
        BackgroundSizeKind::Explicit => pair(
            value.width.map_or_else(|| "auto".into(), lp),
            value.height.map_or_else(|| "auto".into(), lp),
        ),
    }
}
fn gradient_color(value:&Gradient,index:usize)->Option<String> {
    if let Some(metadata)=value.color.as_ref() {
        let color_function=metadata.color_functions&(1<<index)!=0;
        if let Some(color)=metadata.colors.get(index) {return super::color_values::serialize(*color,color_function);}
        if color_function {return super::color_values::serialize(super::color_values::from_rgba(value.stops[index].color),true);}
    }
    Some(color(value.stops[index].color))
}
fn gradient_method(value:&Gradient)->Option<String> {
    use lumen_common::color::{ColorSpace,HueInterpolation,InterpolationMethod};
    let metadata=value.color.as_ref()?;
    let modern=metadata.color_functions!=0 || metadata.colors.iter().any(|color|!matches!(color.space,ColorSpace::Srgb|ColorSpace::Hsl|ColorSpace::Hwb));
    let default=if modern {InterpolationMethod::default()} else {InterpolationMethod{space:ColorSpace::Srgb,hue:HueInterpolation::Shorter}};
    if metadata.method==default {return None;}
    Some(if metadata.method.space.hue().is_some() && metadata.method.hue!=HueInterpolation::Shorter {
        alloc::format!("in {} {} hue",metadata.method.space.name(),metadata.method.hue.name())
    } else {alloc::format!("in {}",metadata.method.space.name())})
}
fn gradient(value: &Gradient) -> Option<String> {
    use crate::paint::GradientPosition;
    let mut result = if value.repeating {
        String::from("repeating-")
    } else {
        String::new()
    };
    match value.kind {
        GradientKind::Linear { angle, corner } => {
            append(&mut result, "linear-gradient(")?;
            if let Some((x, y)) = corner {
                append(&mut result, "to")?;
                if x != 0 {
                    append(&mut result, if x < 0 { " left" } else { " right" })?;
                }
                if y != 0 {
                    append(&mut result, if y > 0 { " top" } else { " bottom" })?;
                }
            } else if angle != 180.0 {
                append(&mut result, &alloc::format!("{}deg", number(angle)))?;
            }
        }
        GradientKind::Radial {
            shape,
            size,
            center,
        } => {
            append(&mut result, "radial-gradient(")?;
            // Explicit radii identify the shape. Omit the default ellipse,
            // farthest-corner size and center when they convey no extra value.
            if shape == RadialShape::Circle && !matches!(size,RadialSize::Radii(_)) {
                append(&mut result,"circle")?;
            }
            let size_text=match size {
                RadialSize::ClosestSide=>Some(String::from("closest-side")),
                RadialSize::FarthestSide=>Some(String::from("farthest-side")),
                RadialSize::ClosestCorner=>Some(String::from("closest-corner")),
                RadialSize::FarthestCorner=>None,
                RadialSize::Radii(values)=>Some(if shape==RadialShape::Circle {lp(values[0])} else {alloc::format!("{} {}",lp(values[0]),lp(values[1]))}),
            };
            if let Some(size)=size_text {
                if !result.ends_with('(') {append(&mut result," ")?;}
                append(&mut result,&size)?;
            }
            if center!=[LengthPercentage {pixels:0.0,fraction:0.5};2] {
                if !result.ends_with('(') {append(&mut result," ")?;}
                append(&mut result,&alloc::format!("at {} {}",lp(center[0]),lp(center[1])))?;
            }
        }
        GradientKind::Conic { from, center } => {
            append(&mut result, "conic-gradient(")?;
            if from != 0.0 { append(&mut result, &alloc::format!("from {}deg", number(from)))?; }
            if center != [LengthPercentage {pixels: 0.0, fraction: 0.5}; 2] {
                if from != 0.0 { append(&mut result, " ")?; }
                append(&mut result, &alloc::format!("at {} {}", lp(center[0]), lp(center[1])))?;
            }
            if let Some(method)=gradient_method(value) {
                if !result.ends_with('(') {append(&mut result," ")?;}append(&mut result,&method)?;
            }
            let mut separator = !result.ends_with('(');
            let mut index = 0;
            // One stop is represented by two identical backend anchors.
            let count = if value.angular.as_ref().is_some_and(|metadata| metadata.single_stop)
                || value.stops.len() == 2 && value.stops[0] == value.stops[1]
                && value.stops[0].position.is_none()
                && value.color.as_ref().is_none_or(|metadata|metadata.colors.is_empty() || metadata.colors[0]==metadata.colors[1]) { 1 } else { value.stops.len() };
            while index < count {
                let stop = &value.stops[index];
                if separator { append(&mut result, ", ")?; }
                separator = true;
                append(&mut result, &gradient_color(value,index)?)?;
                let position = value.angular.as_ref().and_then(|metadata| metadata.positions.get(index)).and_then(Option::as_ref);
                if let Some(position) = position {
                    append(&mut result, " ")?;
                    append(&mut result, &position.serialize()?)?;
                } else if let Some(position) = stop.position {
                    append(&mut result, " ")?;
                    append(&mut result, &match position {
                        GradientPosition::Fraction(value) => alloc::format!("{}%", number(value * 100.0)),
                        _ => return None,
                    })?;
                }
                let hint = value.angular.as_ref().and_then(|metadata| metadata.hints.iter().find(|hint| hint.after == index));
                let second = if hint.is_none() && position.is_some() && index + 1 < count
                    && stop.color == value.stops[index + 1].color
                    && value.color.as_ref().is_none_or(|metadata| metadata.colors.is_empty() || metadata.colors[index]==metadata.colors[index+1]) {
                    value.angular.as_ref().and_then(|metadata| metadata.positions.get(index + 1)).and_then(Option::as_ref)
                } else { None };
                if let Some(second) = second {
                    append(&mut result, " ")?;
                    append(&mut result, &second.serialize()?)?;
                    index += 1;
                }
                // A paired color's outgoing hint belongs after its second position.
                let hint = value.angular.as_ref().and_then(|metadata| metadata.hints.iter().find(|hint| hint.after == index));
                if let Some(hint) = hint {
                    append(&mut result, ", ")?;
                    append(&mut result, &hint.position.serialize()?)?;
                }
                index += 1;
            }
            append(&mut result, ")")?;
            return Some(result);
        },
    }
    if value.stops.len() > 256 {
        return None;
    }
    if let Some(method)=gradient_method(value) {if !result.ends_with('(') {append(&mut result," ")?;}append(&mut result,&method)?;}
    let count=if value.color.as_ref().is_some_and(|metadata|metadata.single_stop){1}else{value.stops.len()};
    for (index,stop) in value.stops.iter().take(count).enumerate() {
        if index!=0 || !result.ends_with('(') {append(&mut result, ", ")?;}
        append(&mut result, &gradient_color(value,index)?)?;
        if let Some(position) = stop.position {
            append(&mut result, " ")?;
            append(
                &mut result,
                &match position {
                    GradientPosition::Pixels(value) => px(value),
                    GradientPosition::Fraction(value) => {
                        alloc::format!("{}%", number(value * 100.0))
                    }
                    GradientPosition::Mixed(value) => lp(value),
                },
            )?;
        }
        if let Some((_,hint))=value.color.as_ref().and_then(|metadata|metadata.hints.iter().find(|(after,_)|*after==index)) {
            append(&mut result,", ")?;
            append(&mut result,&match hint {
                GradientPosition::Pixels(value)=>px(*value),
                GradientPosition::Fraction(value)=>alloc::format!("{}%",number(*value*100.0)),
                GradientPosition::Mixed(value)=>lp(*value),
            })?;
        }
    }
    append(&mut result, ")")?;
    Some(result)
}
pub(super) fn image(value: &BackgroundImage, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    Some(match value {
        BackgroundImage::None => "none".into(),
        BackgroundImage::Solid(value) => alloc::format!("image({})",color(*value)),
        BackgroundImage::Url(url) => alloc::format!("url({})", serialize_css_string(url)),
        BackgroundImage::UrlResolution { url, density } => alloc::format!(
            "image-set(url({}) {}dppx)",
            serialize_css_string(url),
            number(*density)
        ),
        BackgroundImage::Gradient(value) => gradient(value)?,
        BackgroundImage::Paint { name, arguments } => alloc::format!("paint({}{})", name,
            arguments.iter().map(|argument|alloc::format!(", {}",argument)).collect::<String>()),
        BackgroundImage::CrossFade(values) => alloc::format!(
            "cross-fade({})",
            list(values, MAX_BACKGROUND_LAYERS, ", ", |(value, weight)| Some(
                alloc::format!("{} {}%", image(value, depth + 1)?, number(*weight * 100.0))
            ))?
        ),
    })
}
fn breadth(value: GridBreadth) -> String {
    match value {
        GridBreadth::Auto => "auto".into(),
        GridBreadth::MinContent => "min-content".into(),
        GridBreadth::MaxContent => "max-content".into(),
        GridBreadth::Pixels(value) => px(value),
        GridBreadth::Percentage(value) => alloc::format!("{}%", number(value)),
        GridBreadth::Length(px, percent) => percentage(px, percent),
        GridBreadth::Fraction(value) => alloc::format!("{}fr", number(value)),
    }
}
fn track(value: GridTrack) -> String {
    match value {
        GridTrack::Auto => "auto".into(),
        GridTrack::Pixels(value) => px(value),
        GridTrack::Fraction(value) => alloc::format!("{}fr", number(value)),
        GridTrack::Percentage(value) => alloc::format!("{}%", number(value)),
        GridTrack::Length(px, percent) => percentage(px, percent),
        GridTrack::MinContent => "min-content".into(),
        GridTrack::MaxContent => "max-content".into(),
        GridTrack::MinMax(min, max) => alloc::format!("minmax({}, {})", breadth(min), breadth(max)),
        GridTrack::FitContent(value) => alloc::format!("fit-content({})", px(value)),
        GridTrack::FitContentLength(value) => {
            alloc::format!("fit-content({})", percentage(value.pixels, value.percent))
        }
    }
}
struct OrderedNames<'a> {
    names: &'a [GridNamedLine],
    order: Option<Vec<usize>>,
    cursor: usize,
}
impl<'a> OrderedNames<'a> {
    fn new(names: &'a [GridNamedLine]) -> Option<Self> {
        let order = if names.windows(2).all(|pair| pair[0].line <= pair[1].line) {
            None
        } else {
            let mut indices = Vec::new();
            indices.try_reserve_exact(names.len()).ok()?;
            indices.extend(0..names.len());
            indices.sort_unstable_by_key(|index| (names[*index].line, *index));
            Some(indices)
        };
        Some(Self {
            names,
            order,
            cursor: 0,
        })
    }
    fn next_at(&mut self, line: usize) -> Option<&'a GridNamedLine> {
        loop {
            let index = self.order.as_ref().map_or(self.cursor, |order| {
                order.get(self.cursor).copied().unwrap_or(self.names.len())
            });
            let name = self.names.get(index)?;
            if name.line > line {
                return None;
            }
            self.cursor += 1;
            if name.line == line {
                return Some(name);
            }
        }
    }
}
fn named_tracks(
    tracks: &[GridTrack],
    names: &[GridNamedLine],
    used: Option<&[f32]>,
) -> Option<String> {
    if names.len() > (if used.is_some() { 4096 } else { 256 })
        || tracks.len() > MAX_GRID_TRACKS
        || used.is_some_and(|values| values.len() > MAX_GRID_TRACKS)
    {
        return None;
    }
    let mut output = String::new();
    let count = used.map_or(tracks.len(), <[f32]>::len);
    let mut ordered = OrderedNames::new(names)?;
    for line in 0..=count {
        let mut any = false;
        while let Some(name) = ordered.next_at(line) {
            if !any {
                if !output.is_empty() {
                    append(&mut output, " ")?;
                }
                append(&mut output, "[")?;
            } else {
                append(&mut output, " ")?;
            }
            append(&mut output, &serialize_identifier(&name.name))?;
            any = true;
        }
        if any {
            append(&mut output, "]")?;
        }
        if line < count {
            if !output.is_empty() {
                append(&mut output, " ")?;
            }
            append(
                &mut output,
                &if let Some(used) = used {
                    px(used[line])
                } else {
                    track(tracks[line])
                },
            )?;
        }
    }
    Some(if output.is_empty() {
        "none".into()
    } else {
        output
    })
}
pub(super) fn named_line_groups(names: &[GridNamedLine], count: usize) -> Option<String> {
    if count > MAX_GRID_TRACKS + 1 || names.len() > 4096 {
        return None;
    }
    let mut result = String::new();
    let mut ordered = OrderedNames::new(names)?;
    for line in 0..count {
        if line != 0 {
            append(&mut result, " ")?;
        }
        append(&mut result, "[")?;
        let mut any = false;
        while let Some(name) = ordered.next_at(line) {
            if any {
                append(&mut result, " ")?;
            }
            append(&mut result, &serialize_identifier(&name.name))?;
            any = true;
        }
        append(&mut result, "]")?;
    }
    Some(result)
}
pub(super) fn specified_grid_tracks(raw: &str) -> Option<String> {
    let mut result = String::new();
    for token in grid_components(raw)? {
        if let Some(names) = token
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            if names.trim().is_empty() {
                continue;
            }
            grid_append_line_names(&mut result, token)?;
        } else {
            if !result.is_empty() {
                append(&mut result, " ")?;
            }
            append(&mut result, token)?;
        }
    }
    Some(if result.is_empty() {
        "none".into()
    } else {
        result
    })
}
pub(super) fn serialize_grid_flow(flow: GridAutoFlow) -> String {
    match (flow.column, flow.dense) {
        (false, false) => "row",
        (false, true) => "row dense",
        (true, false) => "column",
        (true, true) => "column dense",
    }
    .into()
}
pub(super) fn serialize_lanes_direction(direction: GridLanesDirection) -> String {
    let mut result = direction
        .row
        .map_or("normal", |row| if row { "row" } else { "column" })
        .to_owned();
    if direction.fill_reverse {
        result.push_str(" fill-reverse");
    }
    if direction.track_reverse {
        result.push_str(" track-reverse");
    }
    result
}
fn append_grid_area_row(
    result: &mut String,
    areas: &[GridArea],
    dimensions: [u8; 2],
    row: usize,
) -> Option<()> {
    append(result, "\"")?;
    for column in 0..dimensions[0] as usize {
        if column != 0 {
            append(result, " ")?;
        }
        let area = areas.iter().find(|area| {
            column >= area.column
                && column < area.column + area.columns
                && row >= area.row
                && row < area.row + area.rows
        });
        append(result, area.map_or(".", |area| area.name.as_ref()))?;
    }
    append(result, "\"")
}
pub(super) fn serialize_grid_areas(areas: &[GridArea], dimensions: [u8; 2]) -> Option<String> {
    let [columns, rows] = dimensions;
    if columns == 0 || rows == 0 {
        return Some("none".into());
    }
    if columns as usize > MAX_GRID_TRACKS
        || rows as usize > MAX_GRID_TRACKS
        || areas.len() > MAX_GRID_TRACKS
    {
        return None;
    }
    let mut result = String::new();
    for row in 0..rows as usize {
        if row != 0 {
            append(&mut result, " ")?;
        }
        append_grid_area_row(&mut result, areas, dimensions, row)?;
    }
    Some(result)
}
/// Assemble either specified or resolved Grid template longhands. All track
/// tokens have already passed the shared parser; this operation only pairs
/// area rows with their sizes and boundary names and checks round-trippability.
pub(super) fn serialize_grid_template(rows: &str, columns: &str, areas: &str) -> Option<String> {
    if areas == "none" {
        return Some(if rows == "none" && columns == "none" {
            "none".into()
        } else {
            alloc::format!("{rows} / {columns}")
        });
    }
    let area_rows = grid_components(areas)?;
    let tracks = grid_components(rows)?;
    let mut result = String::new();
    let mut position = 0;
    for area in area_rows {
        if !matches!(area.as_bytes().first(), Some(b'\"' | b'\'')) {
            return None;
        }
        if !result.is_empty() {
            append(&mut result, " ")?;
        }
        if tracks
            .get(position)
            .is_some_and(|value| value.starts_with('['))
        {
            append(&mut result, tracks[position])?;
            append(&mut result, " ")?;
            position += 1;
        }
        let size = *tracks.get(position)?;
        if size.starts_with('[')
            || grid_function_args(size, "repeat").is_some()
            || matches!(size, "none" | "subgrid")
        {
            return None;
        }
        append(&mut result, area)?;
        if size != "auto" {
            append(&mut result, " ")?;
            append(&mut result, size)?;
        }
        position += 1;
    }
    if tracks
        .get(position)
        .is_some_and(|value| value.starts_with('['))
    {
        append(&mut result, " ")?;
        append(&mut result, tracks[position])?;
        position += 1;
    }
    if position != tracks.len() {
        return None;
    }
    if columns != "none" {
        append(&mut result, " / ")?;
        append(&mut result, columns)?;
    }
    Some(result)
}
pub(super) fn serialize_grid_shorthand(values: &[&str]) -> Option<String> {
    let &[rows, columns, areas, auto_rows, auto_columns, flow] = values else {
        return None;
    };
    if auto_rows == "auto" && auto_columns == "auto" && flow == "row" {
        return serialize_grid_template(rows, columns, areas);
    }
    if areas != "none" {
        return None;
    }
    let flow = grid_auto_flow(flow)?;
    let mut automatic = String::from("auto-flow");
    if flow.dense {
        append(&mut automatic, " dense")?;
    }
    if flow.column && columns == "none" && auto_rows == "auto" {
        if auto_columns != "auto" {
            append(&mut automatic, " ")?;
            append(&mut automatic, auto_columns)?;
        }
        Some(alloc::format!("{rows} / {automatic}"))
    } else if !flow.column && rows == "none" && auto_columns == "auto" {
        if auto_rows != "auto" {
            append(&mut automatic, " ")?;
            append(&mut automatic, auto_rows)?;
        }
        Some(alloc::format!("{automatic} / {columns}"))
    } else {
        None
    }
}
pub(super) fn serialize_grid_lanes(values: &[&str]) -> Option<String> {
    let &[rows, columns, areas, direction] = values else {
        return None;
    };
    let parsed = grid_lanes_direction(direction)?;
    let row = parsed.row.unwrap_or(columns == "none" && rows != "none");
    if (if row { columns } else { rows }) != "none" {
        return None;
    }
    let mut result = String::new();
    if areas != "none" {
        let (areas, dimensions) = grid_areas(areas)?;
        let area_text = if row {
            if dimensions[0] != 1 {
                return None;
            }
            let mut transposed = Vec::new();
            transposed.try_reserve_exact(areas.len()).ok()?;
            transposed.extend(areas.iter().map(|area| GridArea {
                name: area.name.clone(),
                row: area.column,
                column: area.row,
                rows: area.columns,
                columns: area.rows,
            }));
            serialize_grid_areas(&transposed, [dimensions[1], dimensions[0]])?
        } else {
            if dimensions[1] != 1 {
                return None;
            }
            serialize_grid_areas(&areas, dimensions)?
        };
        append(&mut result, &area_text)?;
        append(&mut result, " ")?;
    }
    append(&mut result, if row { rows } else { columns })?;
    if parsed.row != Some(false) || parsed.fill_reverse || parsed.track_reverse {
        append(&mut result, " ")?;
        append(&mut result, direction)?;
    }
    Some(result)
}
impl Style {
    fn computed_grid_tracks(
        &self,
        horizontal: bool,
        used: Option<&[f32]>,
        used_names: Option<&[GridNamedLine]>,
    ) -> Option<String> {
        let (tracks, names, subgrid, repeated) = if horizontal {
            (
                self.grid_columns.as_deref(),
                self.grid_column_names.as_deref(),
                self.grid_columns_subgrid,
                self.grid_columns_auto.as_ref(),
            )
        } else {
            (
                self.grid_rows.as_deref(),
                self.grid_row_names.as_deref(),
                self.grid_rows_subgrid,
                self.grid_rows_auto.as_ref(),
            )
        };
        if subgrid {
            let mut result = String::from("subgrid");
            if let Some(used) = used {
                let groups = named_line_groups(
                    used_names.or(names).unwrap_or(&[]),
                    used.len().checked_add(1)?,
                )?;
                if !groups.is_empty() {
                    append(&mut result, " ")?;
                    append(&mut result, &groups)?;
                }
                return Some(result);
            }
            let metadata = if horizontal {
                self.grid_columns_subgrid_repeat.as_ref()
            } else {
                self.grid_rows_subgrid_repeat.as_ref()
            };
            if let Some(metadata) = metadata {
                let prefix = named_line_groups(&metadata.prefix_names, metadata.prefix_lines)?;
                if !prefix.is_empty() {
                    append(&mut result, " ")?;
                    append(&mut result, &prefix)?;
                }
                if metadata.repeat_lines != 0 {
                    append(&mut result, " repeat(auto-fill, ")?;
                    append(
                        &mut result,
                        &named_line_groups(&metadata.repeat_names, metadata.repeat_lines)?,
                    )?;
                    append(&mut result, ")")?;
                }
                let suffix = named_line_groups(&metadata.suffix_names, metadata.suffix_lines)?;
                if !suffix.is_empty() {
                    append(&mut result, " ")?;
                    append(&mut result, &suffix)?;
                }
            } else if let Some(names) = names {
                let count = names.iter().map(|name| name.line + 1).max().unwrap_or(0);
                let groups = named_line_groups(names, count)?;
                if !groups.is_empty() {
                    append(&mut result, " ")?;
                    append(&mut result, &groups)?;
                }
            }
            return Some(result);
        }
        if let Some(used) = used {
            return named_tracks(
                tracks.unwrap_or(&[]),
                used_names.or(names).unwrap_or(&[]),
                Some(used),
            );
        }
        if let Some(repeated) = repeated {
            let prefix = named_tracks(&repeated.prefix_tracks, &repeated.prefix_names, None)?;
            let suffix = named_tracks(&repeated.suffix_tracks, &repeated.suffix_names, None)?;
            let mut result = String::new();
            if prefix != "none" {
                append(&mut result, &prefix)?;
                append(&mut result, " ")?;
            }
            append(
                &mut result,
                &alloc::format!(
                    "repeat({}, {})",
                    if repeated.fit {
                        "auto-fit"
                    } else {
                        "auto-fill"
                    },
                    named_tracks(&repeated.tracks, &repeated.repeat_names, None)?
                ),
            )?;
            if suffix != "none" {
                append(&mut result, " ")?;
                append(&mut result, &suffix)?;
            }
            return Some(result);
        }
        named_tracks(tracks.unwrap_or(&[]), names.unwrap_or(&[]), None)
    }
    fn computed_grid_areas(&self) -> Option<String> {
        serialize_grid_areas(
            self.grid_areas.as_deref().unwrap_or(&[]),
            self.grid_area_dimensions,
        )
    }
    fn computed_complex_css(
        &self,
        name: &str,
        context: ComputedValueContext<'_>,
    ) -> Option<String> {
        Some(match name {
            "animation-name"
            | "animation-duration"
            | "animation-delay-start"
            | "animation-timing-function"
            | "animation-iteration-count"
            | "animation-direction"
            | "animation-fill-mode"
            | "animation-play-state"
            | "animation-composition" => {
                let (index, default) = match name {
                    "animation-name" => (0, "none"),
                    "animation-duration" => (1, "auto"),
                    "animation-delay-start" => (2, "0s"),
                    "animation-timing-function" => (3, "ease"),
                    "animation-iteration-count" => (4, "1"),
                    "animation-direction" => (5, "normal"),
                    "animation-fill-mode" => (6, "none"),
                    "animation-play-state" => (7, "running"),
                    _ => (8, "replace"),
                };
                let value=self.animation[index].as_deref().unwrap_or(default);
                if name=="animation-duration"&&context.resolved&&self.animation[9].as_deref().unwrap_or("auto")=="auto" {
                    top_level_split(value,b',',64)?.into_iter().map(|item|if item=="auto"{"0s"}else{item}).collect::<Vec<_>>().join(", ")
                } else {value.into()}

            }
            "animation-delay-end"=>self.animation[17].as_deref().unwrap_or("0s").into(),
            "animation-delay"=>animation_controls::delay_shorthand(&[self.animation[2].as_deref().unwrap_or("0s"),self.animation[17].as_deref().unwrap_or("0s")])?,
            "animation-range-start"=>self.animation[15].as_deref().unwrap_or("normal").into(),
            "animation-range-end"=>self.animation[16].as_deref().unwrap_or("normal").into(),
            "animation-range"=>animation_controls::range_shorthand(&[self.animation[15].as_deref().unwrap_or("normal"),self.animation[16].as_deref().unwrap_or("normal")])?,
            "animation-timeline"=>self.animation[9].as_deref().unwrap_or("auto").into(),
            "scroll-timeline-name"=>self.animation[11].as_deref().unwrap_or("none").into(),
            "scroll-timeline-axis"=>self.animation[12].as_deref().unwrap_or("block").into(),
            "view-timeline-name"=>self.animation[13].as_deref().unwrap_or("none").into(),
            "view-timeline-axis"=>self.animation[14].as_deref().unwrap_or("block").into(),
            "view-timeline-inset"=>top_level_split(self.animation[18].as_deref().unwrap_or("auto"),b',',64)?.into_iter().map(|part|{
                let tokens=grid_components(part)?;Some(if tokens.len()==1{alloc::format!("{} {}",tokens[0],tokens[0])}else{String::from(part)})
            }).collect::<Option<Vec<_>>>()?.join(", "),

            "scroll-timeline"=>animation_controls::timeline_shorthand(&[self.animation[11].as_deref().unwrap_or("none"),self.animation[12].as_deref().unwrap_or("block")])?,
            "view-timeline"=>animation_controls::timeline_shorthand(&[self.animation[13].as_deref().unwrap_or("none"),self.animation[14].as_deref().unwrap_or("block"),self.animation[18].as_deref().unwrap_or("auto")])?,
            "animation" => self.computed_animation(context.resolved)?,

            "grid-template-columns" => {
                self.computed_grid_tracks(true, context.grid_columns, context.grid_column_names)?
            }
            "grid-template-rows" => {
                self.computed_grid_tracks(false, context.grid_rows, context.grid_row_names)?
            }
            "grid-auto-columns" => named_tracks(
                self.grid_auto_columns
                    .as_deref()
                    .unwrap_or(&[GridTrack::Auto]),
                &[],
                None,
            )?,
            "grid-auto-rows" => named_tracks(
                self.grid_auto_rows.as_deref().unwrap_or(&[GridTrack::Auto]),
                &[],
                None,
            )?,
            "grid-auto-flow" => serialize_grid_flow(self.grid_auto_flow),
            "grid-template-areas" => self.computed_grid_areas()?,
            "grid-column" => self.grid_column_spec.as_deref().unwrap_or("auto").into(),
            "grid-row" => self.grid_row_spec.as_deref().unwrap_or("auto").into(),
            "grid-area" => {
                if let Some(name) = &self.grid_area {
                    serialize_identifier(name)
                } else {
                    let row = self.grid_row_spec.as_deref().unwrap_or("auto / auto");
                    let column = self.grid_column_spec.as_deref().unwrap_or("auto / auto");
                    let rows = top_level_split(row, b'/', 2)?;
                    let columns = top_level_split(column, b'/', 2)?;
                    alloc::format!(
                        "{} / {} / {} / {}",
                        rows[0].trim(),
                        columns[0].trim(),
                        rows.get(1).copied().unwrap_or("auto").trim(),
                        columns.get(1).copied().unwrap_or("auto").trim()
                    )
                }
            }
            "grid-template" | "grid" => {
                let rows =
                    self.computed_grid_tracks(false, context.grid_rows, context.grid_row_names)?;
                let columns = self.computed_grid_tracks(
                    true,
                    context.grid_columns,
                    context.grid_column_names,
                )?;
                let areas = self.computed_grid_areas()?;
                if name == "grid-template" {
                    serialize_grid_template(&rows, &columns, &areas)?
                } else {
                    let auto_rows = named_tracks(
                        self.grid_auto_rows.as_deref().unwrap_or(&[GridTrack::Auto]),
                        &[],
                        None,
                    )?;
                    let auto_columns = named_tracks(
                        self.grid_auto_columns
                            .as_deref()
                            .unwrap_or(&[GridTrack::Auto]),
                        &[],
                        None,
                    )?;
                    serialize_grid_shorthand(&[
                        &rows,
                        &columns,
                        &areas,
                        &auto_rows,
                        &auto_columns,
                        &serialize_grid_flow(self.grid_auto_flow),
                    ])?
                }
            }
            "grid-lanes" => {
                let rows =
                    self.computed_grid_tracks(false, context.grid_rows, context.grid_row_names)?;
                let columns = self.computed_grid_tracks(
                    true,
                    context.grid_columns,
                    context.grid_column_names,
                )?;
                serialize_grid_lanes(&[
                    &rows,
                    &columns,
                    &self.computed_grid_areas()?,
                    &serialize_lanes_direction(self.grid_lanes_direction),
                ])?
            }
            "border-image-source"|"border-image-slice"|"border-image-width"|"border-image-outset"|"border-image-repeat" => {
                let slot=slots(name)[0];self.computed_image_source_value(slot).unwrap_or_else(||border_images::serialize(self.border_image.as_deref().unwrap_or(&BorderImage::default()),slot))
            }
            "border-image"=>{let initial=BorderImage::default();let value=self.border_image.as_deref().unwrap_or(&initial);border_images::serialize_shorthand(&self.computed_image_source_value(227).unwrap_or_else(||border_images::serialize(value,227)),&border_images::serialize(value,228),&border_images::serialize(value,229),&border_images::serialize(value,230),&border_images::serialize(value,231))},
            "background-image" => self.computed_image_source_value(53).or_else(||self.background_images.as_deref().map_or_else(
                || Some("none".into()),
                |values| list(values, MAX_BACKGROUND_LAYERS, ", ", |value| image(value, 0)),
            ))?,
            "background-position" => background_positions_css_value(self.background_position.as_deref())?,
            "background-size" => self.background_sizes_computed_value()?,
            "background-repeat" => self.background_repeat.as_deref().map_or_else(
                || Some("repeat".into()),
                |values| {
                    list(values, MAX_BACKGROUND_LAYERS, ", ", |value| {
                        Some(declaration_block::serialize_background_repeat_pair(*value))
                    })
                },
            )?,
            "background-clip" => self.background_clip.as_deref().map_or_else(
                || Some("border-box".into()),
                |values| {
                    list(values, MAX_BACKGROUND_LAYERS, ", ", |value| {
                        Some(background_box(*value))
                    })
                },
            )?,
            "background-origin" => self.background_origin.as_deref().map_or_else(
                || Some("padding-box".into()),
                |values| {
                    list(values, MAX_BACKGROUND_LAYERS, ", ", |value| {
                        Some(background_box(*value))
                    })
                },
            )?,
            "background-attachment" => self.background_attachment.as_deref().map_or_else(
                || Some("scroll".into()),
                |values| {
                    list(values, MAX_BACKGROUND_LAYERS, ", ", |value| {
                        Some(attachment(*value).into())
                    })
                },
            )?,
            "background" => self.computed_background(context.resolved)?,
            "box-shadow" => if context.resolved {
                match self.source_shadow_list(58) {
                    Some(source) if source.colors.is_some()=>crate::animation::transition_values::serialize_source_shadow_list(name,&source)?,
                    _=>shadows_css_value(self.shadows.as_deref())?,
                }
            }else{self.source_shadows_css_value(name)?},
            "text-shadow" => self.source_shadows_css_value(name)?,
            "transform-origin" => {
                let values = self.transform_origin;
                if let Some(rect) = transform_reference(self,context) {
                    alloc::format!(
                        "{} {}",
                        px(values[0].resolve(rect.width)),
                        px(values[1].resolve(rect.height))
                    )
                } else {
                    alloc::format!(
                        "{} {}",
                        percentage(values[0].pixels, values[0].percent),
                        percentage(values[1].pixels, values[1].percent)
                    )
                }
            }
            "translate"|"rotate"|"scale"=>{
                let index=match name{"translate"=>0,"rotate"=>1,_=>2};
                self.individual_transforms[index].as_ref().map_or_else(||Some("none".into()),|value|value.computed_css_value())?
            },
            "transform-box"=>self.transform_box.keyword().into(),
            "transform" => {
                let reference=transform_reference(self,context);
                if let Some(source)=self.relative_expressions.iter().find(|expression|expression.slot==59) {
                    return Some(if context.resolved {
                        if context.transform_timeline_source==Some("none"){return Some("none".into());}
                        if let Some(rect)=reference {
                            typed_transforms::matrix_css_value(typed_transforms::computed_matrix(context.transform_timeline_source.unwrap_or(&source.raw),Some([rect.width as f64,rect.height as f64]))?)?
                        }else{source.raw.to_string()}
                    }else{source.raw.to_string()});
                }
                let Some(values) = self
                    .transforms
                    .as_deref()
                    .filter(|values| !values.is_empty())
                else {
                    return Some("none".into());
                };
                if !context.resolved || (reference.is_none() && values.iter().any(|value| matches!(value, Transform::Translate(x, y) if x.percent != 0.0 || y.percent != 0.0))) { list(values, 64, " ", |value| Some(match value { Transform::Matrix(v) => alloc::format!("matrix({}, {}, {}, {}, {}, {})", number(v.a), number(v.b), number(v.c), number(v.d), number(v.e), number(v.f)), Transform::Translate(x, y) => alloc::format!("translate({}, {})", percentage(x.pixels, x.percent), percentage(y.pixels, y.percent)), Transform::Scale(x, y) => alloc::format!("scale({}, {})", number(*x), number(*y)), Transform::Rotate(value) => alloc::format!("rotate({}deg)", number(*value * 180.0 / core::f32::consts::PI)), Transform::Skew(x, y) => alloc::format!("skew({}deg, {}deg)", number(*x * 180.0 / core::f32::consts::PI), number(*y * 180.0 / core::f32::consts::PI)) }))? } else { let rect = reference.unwrap_or(Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }); let matrix = values.iter().fold(Affine::IDENTITY, |matrix, value| matrix.then(value.matrix(rect.width, rect.height))); alloc::format!("matrix({}, {}, {}, {}, {}, {})", number(matrix.a), number(matrix.b), number(matrix.c), number(matrix.d), number(matrix.e), number(matrix.f)) }
            }
            _ => return None,
        })
    }
    fn computed_animation(&self, resolved:bool) -> Option<String> {
        let defaults=["none","auto","0s","ease","1","normal","none","running","replace","auto","normal","normal","0s"];
        let indexes=[0,1,2,3,4,5,6,7,8,9,15,16,17];
        let values=core::array::from_fn::<_,13,_>(|index|self.animation[indexes[index]].as_deref().unwrap_or(defaults[index]));
        let resolved_duration=resolved&&values[9]=="auto";
        let duration=resolved_duration.then(||top_level_split(values[1],b',',64).map(|items|items.into_iter().map(|item|if item=="auto"{"0s"}else{item}).collect::<Vec<_>>().join(", "))).flatten();
        let mut values=values;
        if let Some(duration)=duration.as_deref(){values[1]=duration;}
        animation_controls::shorthand_with_resolution(&values,resolved_duration)

    }
    fn computed_background(&self,resolved:bool) -> Option<String> {
        let default_images = [BackgroundImage::None];
        let images = self.background_images.as_deref().unwrap_or(&default_images);
        let mut output = String::new();
        if images.len() > MAX_BACKGROUND_LAYERS {
            return None;
        }
        for (index, value) in images.iter().enumerate() {
            if index != 0 {
                append(&mut output, ", ")?;
            }
            let position = self
                .background_position
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or([LengthPercentage::default(); 2]);
            let size = self
                .background_size
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or(BackgroundSize::AUTO);
            let repeat = self
                .background_repeat
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or([BackgroundRepeat::Repeat; 2]);
            let origin = self
                .background_origin
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or(BackgroundBox::Padding);
            let clip = self
                .background_clip
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or(BackgroundBox::Border);
            let attach = self
                .background_attachment
                .as_deref()
                .and_then(|values| (!values.is_empty()).then(|| values[index % values.len()]))
                .unwrap_or(BackgroundAttachment::Scroll);
            append(
                &mut output,
                &alloc::format!(
                    "{} {} {} / {} {} {} {} {}",
                    image(value, 0)?,
                    lp(position[0]),
                    lp(position[1]),
                    background_size(&size),
                    declaration_block::serialize_background_repeat_pair(repeat),
                    attachment(attach),
                    background_box(origin),
                    background_box(clip)
                ),
            )?;
            if index + 1 == images.len() {
                append(&mut output, " ")?;
                append(&mut output, &style_color_for_phase(self,2,self.background,resolved))?;
            }
        }
        Some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn styled(source: &str) -> Style {
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: vec![("style".into(), source.into())],
        };
        compute(&kind, None, &StyleIndex::new(Vec::new())).unwrap()
    }
    fn value(style: &Style, name: &str) -> String {
        style
            .computed_css_value(name, ComputedValueContext::default())
            .unwrap_or_else(|| panic!("missing computed property: {name}"))
    }

    #[test]
    fn specification_text_decoration_shorthand_omits_initial_color_after_resolution() {
        for (raw,expected) in [("none","none"),("solid","none"),("currentcolor","none"),
            ("line-through","line-through"),("double overline underline","underline overline double"),
            ("10px","10px"),("underline blue","underline rgb(0, 0, 255)")] {
            let style=styled(&alloc::format!("color:blue;text-decoration:{raw}"));
            assert_eq!(value(&style,"text-decoration"),expected,"{raw}: initial keyword ownership survives its used color projection");
            assert_eq!(value(&style,"text-decoration-color"),"rgb(0, 0, 255)");
        }
        let style=styled("color:blue;text-decoration:underline;text-decoration-color:rgb(0, 0, 255)");
        assert_eq!(value(&style,"text-decoration"),"underline rgb(0, 0, 255)","an explicit color equal to the foreground remains explicit");
        let style=styled("color:blue;text-decoration:underline red;text-decoration-color:currentcolor");
        assert_eq!(value(&style,"text-decoration"),"underline","a later initial keyword replaces an explicit color");
        let style=styled("text-decoration:underline red;all:initial;color:red");
        assert_eq!(value(&style,"text-decoration"),"none");
        for property in ["text-decoration-color","column-rule-color","outline-color"] {
            assert_eq!(value(&style,property),"rgb(255, 0, 0)","{property}: copying an initial keyword keeps the later foreground dependency");
        }
    }

    #[test]
    fn computed_values_outline_resolves_current_color_width_and_preserves_auto() {
        let mut style = Style::initial();
        style.color = Rgba {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        };
        assert_eq!(value(&style, "outline-width"), "0px");
        style.outline = Outline {
            width: 2.0,
            style: OutlineStyle::Border(BorderStyle::Dotted),
            color: None,
            offset: -1.0,
        };
        assert_eq!(value(&style, "outline"), "rgb(0, 0, 255) dotted 2px");
        assert_eq!(value(&style, "outline-offset"), "-1px");
        style.outline.style = OutlineStyle::Auto;
        assert_eq!(value(&style, "outline-style"), "auto");
        assert_eq!(value(&style, "outline-width"), "2px");
    }

    #[test]
    fn computed_values_border_sides_hidden_and_column_rule_preserve_real_style() {
        let style = styled("border:5px solid red;border-right:2px dotted blue;border-left-style:hidden;column-rule:thick hidden green");
        assert_eq!(value(&style, "border-width"), "5px 2px 5px 0px");
        assert_eq!(value(&style, "border-style"), "solid dotted solid hidden");
        assert_eq!(value(&style, "border-right-color"), "rgb(0, 0, 255)");
        assert_eq!(value(&style, "border-left-width"), "0px");
        assert!(style
            .computed_css_value("border", ComputedValueContext::default())
            .is_none());
        assert_eq!(value(&style, "column-rule-style"), "hidden");
        assert_eq!(value(&style, "column-rule-width"), "0px");
        assert_eq!(style.used_border_widths(), [5.0, 2.0, 5.0, 0.0]);
    }

    #[test]
    fn specification_resolved_content_dimensions_subtract_fractional_box_offsets_once() {
        let style=styled("width:0;height:0;padding:5.6px 10px 10px 5.6px;border:2px solid");
        for (extent,expected) in [(19.6,"0px"),(19.85,"0.25px")] {
            let context=ComputedValueContext{border_box:Some(Rect{x:0.0,y:0.0,width:extent,height:extent}),..Default::default()};
            for property in ["width","height"] {
                assert_eq!(style.computed_css_value(property,context).unwrap(),expected);
            }
            let mut border_box=style.clone();border_box.box_sizing=BoxSizing::BorderBox;
            assert_eq!(border_box.computed_css_value("width",context).unwrap(),px(extent));
        }
    }

    #[test]
    fn computed_values_percentages_use_layout_context_and_content_box_edges() {
        let style = styled(
            "width:50%;height:30px;padding:10%;margin-left:calc(5% + 2px);border:thick hidden",
        );
        assert_eq!(value(&style, "width"), "50%");
        assert_eq!(value(&style, "padding-left"), "10%");
        let context = ComputedValueContext {
            border_box: Some(Rect {
                x: 700.0,
                y: 900.0,
                width: 140.0,
                height: 70.0,
            }),
            percentage_basis: Some((200.0, Some(100.0))),
            ..Default::default()
        };
        assert_eq!(style.computed_css_value("width", context).unwrap(), "100px");
        assert_eq!(style.computed_css_value("height", context).unwrap(), "30px");
        assert_eq!(
            style.computed_css_value("padding-left", context).unwrap(),
            "20px"
        );
        assert_eq!(
            style.computed_css_value("margin-left", context).unwrap(),
            "12px"
        );
        let mut border_box = style.clone();
        border_box.box_sizing = BoxSizing::BorderBox;
        assert_eq!(
            border_box.computed_css_value("width", context).unwrap(),
            "140px"
        );
        let inline = styled("display:inline;width:50%");
        assert_eq!(inline.computed_css_value("width", context).unwrap(), "50%");
    }

    #[test]
    fn computed_values_logical_edges_case_fonts_and_gap_distinctions() {
        let style = styled("writing-mode:vertical-rl;direction:rtl;margin:1px 2px 3px 4px;padding:5px 6px 7px 8px;inset:9px 10px 11px 12px;inline-size:20px;font:italic bold 20px/1.5 serif;gap:normal 0px;--Case:UP;--case:down");
        assert_eq!(value(&style, "margin-inline-start"), "3px");
        assert_eq!(value(&style, "padding-block-start"), "6px");
        assert_eq!(value(&style, "inset-inline-end"), "9px");
        assert_eq!(value(&style, "inline-size"), "20px");
        assert_eq!(value(&style, "FONT-FAMILY"), "serif");
        assert_eq!(value(&style, "line-height"), "1.5");
        assert_eq!(style.computed_css_value("line-height", ComputedValueContext {
            resolved: true, ..Default::default()
        }).unwrap(), "30px");
        assert_eq!(value(&style, "row-gap"), "normal");
        assert_eq!(value(&style, "column-gap"), "0px");
        assert_eq!(value(&style, "--Case"), "UP");
        assert_eq!(value(&style, "--case"), "down");
        assert_eq!(value(&styled("row-gap:0px"), "row-gap"), "0px");
        assert_eq!(value(&style, "font"), "italic 700 20px / 1.5 serif");
    }

    #[test]
    fn computed_width_aliases_share_percentage_values_and_shorthand_boundaries() {
        for (specified,expected) in [("condensed","75%"),("calc(100% + 100%)","200%"),("calc(-100%)","0%"),("234.5%","234.5%")] {
            let style = styled(&alloc::format!("font-width:{specified}"));
            assert_eq!(value(&style,"font-width"),expected);
            assert_eq!(value(&style,"font-stretch"),expected);
        }
        let style = styled("font-width:76%");
        assert!(style.computed_css_value("font",ComputedValueContext::default()).is_none());
    }

    #[test]
    fn computed_feature_maps_serialize_order_and_preserve_lossless_font_shorthand() {
        let style = styled("font-feature-settings:'tnum', 'hist', 'tnum' off");
        assert_eq!(value(&style,"font-feature-settings"),"\"hist\", \"tnum\" 0");
        assert!(style.computed_css_value("font",ComputedValueContext::default()).is_none());
        let reset = styled("font-feature-settings:'liga' off;font:16px serif");
        assert_eq!(value(&reset,"font-feature-settings"),"normal");
        assert_eq!(value(&reset,"font"),"16px serif");
        let preserved = styled("font-feature-settings:'liga' off;font-variant:normal");
        assert_eq!(value(&preserved,"font-feature-settings"),"\"liga\" 0");
    }

    #[test]
    fn computed_ligature_controls_serialize_groups_and_reset_only_font_state() {
        let style = styled("font-variant:small-caps contextual discretionary-ligatures no-common-ligatures");
        assert_eq!(value(&style, "font-variant-ligatures"), "no-common-ligatures discretionary-ligatures contextual");
        assert_eq!(value(&style, "font-variant"), "no-common-ligatures discretionary-ligatures contextual small-caps");
        assert_eq!(value(&styled("font-variant:none"), "font-variant"), "none");
        assert!(styled("font:16px serif;font-variant-ligatures:none").computed_css_value("font", ComputedValueContext::default()).is_none());
        assert_eq!(value(&styled("font-variant:none;font:16px serif"), "font-variant-ligatures"), "normal");
    }

    #[test]
    fn computed_values_oblique_angles_canonicalize_and_round_trip() {
        for (specified, expected) in [("oblique 10grad", "oblique 9deg"),
            ("oblique calc(100deg)", "oblique 90deg"), ("oblique 0deg", "normal"),
            ("oblique -25deg", "oblique -25deg")] {
            let style = styled(&alloc::format!("font-style:{specified}"));
            assert_eq!(value(&style, "font-style"), expected, "{specified}");
            let reparsed = styled(&alloc::format!("font-style:{}", value(&style, "font-style")));
            assert_eq!(reparsed.font.style, style.font.style);
        }
        let style = styled("font:oblique 25deg bold 16px serif");
        let serialized = value(&style, "font");
        assert_eq!(styled(&alloc::format!("font:{serialized}")).font, style.font);
    }

    #[test]
    fn computed_values_font_keyword_and_system_shorthands_round_trip() {
        for specified in ["italic small-caps bold condensed xx-large/1.5 FB Armada, serif",
            "caption", "icon", "menu", "message-box", "small-caption", "status-bar",
            "0px/12px emoji, math"] {
            let style = styled(&alloc::format!("font:{specified}"));
            let serialized = value(&style, "font");
            assert!(!serialized.is_empty(), "{specified}");
            let reparsed = styled(&alloc::format!("font:{serialized}"));
            assert_eq!(value(&reparsed, "font"), serialized);
            assert_eq!(value(&reparsed, "font-size"), value(&style, "font-size"));
            assert_eq!(value(&reparsed, "line-height"), value(&style, "line-height"));
        }
        assert_eq!(value(&styled("font-size:xxx-large"), "font-size"), "48px");
        assert_eq!(value(&styled("font-size:calc(10px - 2em)"), "font-size"), "0px");
        assert_eq!(value(&styled("font:small-caps large/0 sans-serif"), "font"), "small-caps 19.2px / 0px sans-serif");
    }

    #[test]
    fn computed_values_grid_shapes_names_auto_repeat_and_bounded_used_tracks() {
        let style = styled(
            r#"grid-template-areas: "a ." ". .";grid-template-columns:[start] 10px [middle] minmax(1em,1fr) [end]"#,
        );
        assert_eq!(value(&style, "grid-template-areas"), r#""a ." ". .""#);
        assert_eq!(
            value(&style, "grid-template-columns"),
            "[start] 10px [middle] minmax(16px, 1fr) [end]"
        );
        let context = ComputedValueContext {
            grid_columns: Some(&[25.0, 75.0]),
            ..Default::default()
        };
        assert_eq!(
            style
                .computed_css_value("grid-template-columns", context)
                .unwrap(),
            "[start] 25px [middle] 75px [end]"
        );
        let dots = styled(r#"grid-template-areas: ". . ." ". . .""#);
        assert_eq!(value(&dots, "grid-template-areas"), r#"". . ." ". . .""#);
        let repeated = styled("grid-template-columns:10px repeat(auto-fit,20px) 30px");
        assert_eq!(
            value(&repeated, "grid-template-columns"),
            "10px repeat(auto-fit, 20px) 30px"
        );
        let subgrid =
            styled("grid-template-columns:subgrid [a] [] repeat(auto-fill,[b] []) [c] []");
        assert_eq!(
            value(&subgrid, "grid-template-columns"),
            "subgrid [a] [] repeat(auto-fill, [b] []) [c] []"
        );
        let fixed = styled("grid-template-columns:subgrid [a] [] []");
        assert_eq!(value(&fixed, "grid-template-columns"), "subgrid [a] [] []");
        let expanded_names = [
            GridNamedLine {
                line: 2,
                name: "end".into(),
            },
            GridNamedLine {
                line: 0,
                name: "expanded".into(),
            },
        ];
        let expanded = ComputedValueContext {
            grid_columns: Some(&[25.0, 75.0]),
            grid_column_names: Some(&expanded_names),
            ..Default::default()
        };
        assert_eq!(
            repeated
                .computed_css_value("grid-template-columns", expanded)
                .unwrap(),
            "[expanded] 25px 75px [end]"
        );
        assert_eq!(
            subgrid
                .computed_css_value("grid-template-columns", expanded)
                .unwrap(),
            "subgrid [expanded] [] [end]"
        );
        assert!(style
            .computed_css_value(
                "grid-template-columns",
                ComputedValueContext {
                    grid_columns: Some(&[0.0; MAX_GRID_TRACKS + 1]),
                    ..Default::default()
                }
            )
            .is_none());
    }

    #[test]
    fn computed_values_background_transform_svg_and_alpha_share_real_state() {
        let style = styled("background-image:linear-gradient(90deg,red,blue);background-repeat:no-repeat repeat;background-size:cover;background-color:rgba(1,2,3,.5);transform:translate(50%, 10px) scale(2);transform-origin:25% 75%;fill:currentColor;stroke:blue;color:red;rx:3px;r:4px");
        assert_eq!(value(&style, "background-repeat"), "repeat-y");
        assert_eq!(value(&style, "background-size"), "cover");
        assert_eq!(value(&style, "background-color"), "rgba(1, 2, 3, 0.5)");
        assert!(value(&style, "background-image")
            .starts_with("linear-gradient(90deg, rgb(255, 0, 0), rgb(0, 0, 255))"));
        let context = ComputedValueContext {
            resolved: true,
            border_box: Some(Rect {
                x: 900.0,
                y: 800.0,
                width: 100.0,
                height: 40.0,
            }),
            ..Default::default()
        };
        assert_eq!(
            style
                .computed_css_value("transform-origin", context)
                .unwrap(),
            "25px 30px"
        );
        assert!(style
            .computed_css_value("transform", context)
            .unwrap()
            .starts_with("matrix(2, 0, 0, 2,"));
        assert_eq!(value(&style, "fill"), "rgb(255, 0, 0)");
        assert_eq!(value(&style, "stroke"), "rgb(0, 0, 255)");
        assert_eq!(value(&style, "rx"), "3px");
        assert_eq!(value(&style, "r"), "4px");
        let content=styled("display:block;padding:10%;border:5px solid;transform-box:content-box;transform-origin:50% 50%;transform:translate(50%,100%)");
        let content_context=ComputedValueContext {
            resolved:true,border_box:Some(Rect{x:0.0,y:0.0,width:100.0,height:60.0}),
            percentage_basis:Some((200.0,Some(100.0))),..Default::default()
        };
        assert_eq!(content.computed_css_value("transform-origin",content_context).unwrap(),"25px 5px",
            "content reference resolves percentage padding against its actual containing block");
        assert_eq!(content.computed_css_value("transform",content_context).unwrap(),"matrix(1, 0, 0, 1, 25, 10)");
        assert!(!needs_layout("color"));
        assert!(needs_layout("padding-left"));
    }

    #[test]
    fn specification_animation_controls_computed_and_resolved_duration_keep_timeline_identity() {
        let resolved=ComputedValueContext{resolved:true,..Default::default()};
        let computed=ComputedValueContext::default();
        let style=styled("animation-duration:auto,auto");
        assert_eq!(style.computed_css_value("animation-duration",computed).as_deref(),Some("auto, auto"));
        assert_eq!(style.computed_css_value("animation-duration",resolved).as_deref(),Some("0s, 0s"));
        for timeline in ["auto, auto","none","--named","scroll()","view()"] {
            let style=styled(&alloc::format!("animation-duration:auto;animation-timeline:{timeline}"));
            assert_eq!(style.computed_css_value("animation-duration",resolved).as_deref(),Some("auto"),"{timeline}");
        }
        for (raw,expected) in [("animation:none","none"),("animation:1s","1s"),("animation:ease-in","ease-in"),("animation-delay-start:1s","0s 1s"),("animation:spin 1s","1s spin")] {
            let style=styled(raw);
            assert_eq!(style.computed_css_value("animation",resolved).as_deref(),Some(expected),"{raw}");
        }
        let style=styled("animation-duration:auto;animation-timeline:scroll();animation-range:entry calc(1em + 10%) exit 120%");
        assert_eq!(style.computed_css_value("animation-duration",resolved).as_deref(),Some("auto"));
        assert_eq!(style.computed_css_value("animation-range-end",computed).as_deref(),Some("exit 120%"));
    }

    #[test]
    fn computed_values_animation_lists_reuse_component_boundaries() {
        let style = styled("animation:spin 2s cubic-bezier(.1,.2,.3,.4), pulse 3s steps(2,end)");
        let serialized = value(&style, "animation");
        assert!(serialized.contains("cubic-bezier(.1,.2,.3,.4)"));
        assert!(serialized.contains("steps(2,end)"));
        assert_eq!(top_level_split(&serialized, b',', 64).unwrap().len(), 2);
        let mismatched = styled("animation-name:spin,pulse;animation-duration:2s");
        assert!(mismatched
            .computed_css_value("animation", ComputedValueContext::default())
            .is_none());
    }

    #[test]
    fn computed_values_insets_distinguish_static_no_box_and_relative_auto_pairs() {
        let context = ComputedValueContext {
            percentage_basis: Some((200.0, Some(100.0))),
            border_box: Some(Rect {
                x: 7.0,
                y: 9.0,
                width: 10.0,
                height: 10.0,
            }),
            ..Default::default()
        };
        let static_style = styled("top:10%;left:calc(25% - 2px)");
        assert_eq!(
            static_style.computed_css_value("top", context).unwrap(),
            "10%"
        );
        assert_eq!(
            static_style.computed_css_value("right", context).unwrap(),
            "auto"
        );
        let relative = styled("position:relative;top:auto;bottom:25%;left:20%;right:auto");
        assert_eq!(
            relative.computed_css_value("top", context).unwrap(),
            "-25px"
        );
        assert_eq!(
            relative.computed_css_value("bottom", context).unwrap(),
            "25px"
        );
        assert_eq!(
            relative.computed_css_value("left", context).unwrap(),
            "40px"
        );
        assert_eq!(
            relative.computed_css_value("right", context).unwrap(),
            "-40px"
        );
        assert_eq!(value(&relative, "top"), "auto");
        assert_eq!(value(&relative, "bottom"), "25%");
        let no_box = styled("position:relative;display:none;left:20%;right:auto");
        assert_eq!(no_box.computed_css_value("left", context).unwrap(), "20%");
        assert_eq!(no_box.computed_css_value("right", context).unwrap(), "auto");
        let both = styled("position:relative;left:2px;right:4px");
        assert_eq!(both.computed_css_value("left", context).unwrap(), "2px");
        assert_eq!(both.computed_css_value("right", context).unwrap(), "4px");
        assert_eq!(both.computed_css_value("top", context).unwrap(), "0px");
    }

    #[test]
    fn computed_values_grid_area_shorthands_share_names_and_resolved_tracks() {
        let style =
            styled("grid-template:[head] \"a a\" auto [middle] \"b b\" 1em [end] / 1fr 2fr");
        assert_eq!(
            value(&style, "grid-template"),
            "[head] \"a a\" [middle] \"b b\" 16px [end] / 1fr 2fr"
        );
        let context = ComputedValueContext {
            grid_rows: Some(&[30.0, 40.0]),
            grid_columns: Some(&[100.0, 200.0]),
            ..Default::default()
        };
        assert_eq!(
            style.computed_css_value("grid-template", context).unwrap(),
            "[head] \"a a\" 30px [middle] \"b b\" 40px [end] / 100px 200px"
        );
        assert!(style
            .computed_css_value(
                "grid-template",
                ComputedValueContext {
                    grid_rows: Some(&[30.0, 40.0, 50.0]),
                    ..context
                }
            )
            .is_none());
        let automatic = styled("font-size:20px;grid:auto-flow 1em / 10px");
        assert_eq!(value(&automatic, "grid-auto-rows"), "20px");
        assert_eq!(value(&automatic, "grid"), "auto-flow 20px / 10px");
    }

    #[test]
    fn computed_values_initial_registry_longhands_have_real_serialization() {
        let style = Style::initial();
        for property in cssom_property_names()
            .into_iter()
            .filter(|property| !declaration_block::is_shorthand(property))
        {
            assert!(
                style
                    .computed_css_value(property, ComputedValueContext::default())
                    .is_some(),
                "missing real state serialization for {property}"
            );
        }
        assert!(style
            .computed_css_value("made-up-width", ComputedValueContext::default())
            .is_none());
        assert_eq!(value(&style, "min-width"), "auto");
        assert_eq!(value(&style, "row-gap"), "normal");
    }
}
