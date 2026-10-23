use super::*;

pub(crate) const BASE: usize = 265;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Value(usize),
    TabSize,
    TextSizeAdjust,
    Keywords(&'static [&'static str]),
    Perspective,
    Position,
    HyphenChars,
    HyphenLines,
    ContentVisibility,
    FilterList,
    MaskBorder(usize),
    Anchor,
    Zoom,
    Shape(shapes::Prop),
    Gap(gap_rules::Part),
    Corner,
    BgPos(background_positions::Axis),
    Text,
    GridLine,
    MathDepth,
}

#[derive(Clone, Copy)]
struct Prop {
    name: &'static str,
    inherited: bool,
    initial: &'static str,
    kind: Kind,
}

const PROPS: &[Prop] = &[
    Prop { name: "tab-size", inherited: true, initial: "8", kind: Kind::TabSize },
    Prop { name: "text-size-adjust", inherited: true, initial: "auto", kind: Kind::TextSizeAdjust },
    Prop { name: "text-justify", inherited: true, initial: "auto",
        kind: Kind::Keywords(&["auto", "none", "inter-word", "inter-character", "distribute"]) },
    Prop { name: "text-align-last", inherited: true, initial: "auto",
        kind: Kind::Keywords(&["auto", "start", "end", "left", "right", "center", "justify", "match-parent"]) },
    Prop { name: "perspective", inherited: false, initial: "none", kind: Kind::Perspective },
    Prop { name: "perspective-origin", inherited: false, initial: "50% 50%", kind: Kind::Position },
    Prop { name: "hyphenate-limit-chars", inherited: true, initial: "auto", kind: Kind::HyphenChars },
    Prop { name: "hyphenate-limit-lines", inherited: true, initial: "no-limit", kind: Kind::HyphenLines },
    Prop { name: "content-visibility", inherited: false, initial: "visible", kind: Kind::ContentVisibility },
    Prop { name: "overlay", inherited: false, initial: "none", kind: Kind::Keywords(&["none", "auto"]) },
    Prop { name: "object-position", inherited: false, initial: "50% 50%", kind: Kind::Position },
    Prop { name: "backdrop-filter", inherited: false, initial: "none", kind: Kind::FilterList },
    Prop { name: "mask-border-source", inherited: false, initial: "none", kind: Kind::MaskBorder(0) },
    Prop { name: "mask-border-slice", inherited: false, initial: "0", kind: Kind::MaskBorder(1) },
    Prop { name: "mask-border-width", inherited: false, initial: "auto", kind: Kind::MaskBorder(2) },
    Prop { name: "mask-border-outset", inherited: false, initial: "0", kind: Kind::MaskBorder(3) },
    Prop { name: "mask-border-repeat", inherited: false, initial: "stretch", kind: Kind::MaskBorder(4) },
    Prop { name: "mask-border-mode", inherited: false, initial: "alpha", kind: Kind::Keywords(&["luminance", "alpha"]) },
    Prop { name: "anchor-name", inherited: false, initial: "none", kind: Kind::Anchor },
    Prop { name: "anchor-scope", inherited: false, initial: "none", kind: Kind::Anchor },
    Prop { name: "position-anchor", inherited: false, initial: "normal", kind: Kind::Anchor },
    Prop { name: "position-area", inherited: false, initial: "none", kind: Kind::Anchor },
    Prop { name: "position-try-fallbacks", inherited: false, initial: "none", kind: Kind::Anchor },
    Prop { name: "position-try-order", inherited: false, initial: "normal", kind: Kind::Anchor },
    Prop { name: "position-visibility", inherited: false, initial: "anchor-visible", kind: Kind::Anchor },
    Prop { name: "interpolate-size", inherited: true, initial: "numeric-only", kind: Kind::Keywords(&["numeric-only", "allow-keywords"]) },
    Prop { name: "line-break", inherited: true, initial: "auto", kind: Kind::Keywords(&["auto", "loose", "normal", "strict", "anywhere"]) },
    Prop { name: "clip-path", inherited: false, initial: "none", kind: Kind::Shape(shapes::Prop::ClipPath) },
    Prop { name: "shape-outside", inherited: false, initial: "none", kind: Kind::Shape(shapes::Prop::ShapeOutside) },
    Prop { name: "shape-margin", inherited: false, initial: "0px", kind: Kind::Shape(shapes::Prop::ShapeMargin) },
    Prop { name: "shape-image-threshold", inherited: false, initial: "0", kind: Kind::Shape(shapes::Prop::ShapeImageThreshold) },
    Prop { name: "offset-path", inherited: false, initial: "none", kind: Kind::Shape(shapes::Prop::OffsetPath) },
    Prop { name: "offset-distance", inherited: false, initial: "0px", kind: Kind::Shape(shapes::Prop::OffsetDistance) },
    Prop { name: "offset-rotate", inherited: false, initial: "auto", kind: Kind::Shape(shapes::Prop::OffsetRotate) },
    Prop { name: "offset-anchor", inherited: false, initial: "auto", kind: Kind::Shape(shapes::Prop::OffsetAnchor) },
    Prop { name: "offset-position", inherited: false, initial: "normal", kind: Kind::Shape(shapes::Prop::OffsetPosition) },
    Prop { name: "clip", inherited: false, initial: "auto", kind: Kind::Shape(shapes::Prop::Clip) },
    Prop { name: "zoom", inherited: false, initial: "1", kind: Kind::Zoom },
    Prop { name: "column-rule-width", inherited: false, initial: "3px", kind: Kind::Gap(gap_rules::Part::Width) },
    Prop { name: "row-rule-width", inherited: false, initial: "3px", kind: Kind::Gap(gap_rules::Part::Width) },
    Prop { name: "column-rule-color", inherited: false, initial: "currentcolor", kind: Kind::Gap(gap_rules::Part::Color) },
    Prop { name: "row-rule-color", inherited: false, initial: "currentcolor", kind: Kind::Gap(gap_rules::Part::Color) },
    Prop { name: "column-rule-style", inherited: false, initial: "none", kind: Kind::Gap(gap_rules::Part::Style) },
    Prop { name: "row-rule-style", inherited: false, initial: "none", kind: Kind::Gap(gap_rules::Part::Style) },
    Prop { name: "column-rule-inset-cap-start", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "column-rule-inset-cap-end", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "column-rule-inset-junction-start", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "column-rule-inset-junction-end", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "row-rule-inset-cap-start", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "row-rule-inset-cap-end", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "row-rule-inset-junction-start", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "row-rule-inset-junction-end", inherited: false, initial: "0px", kind: Kind::Gap(gap_rules::Part::Inset) },
    Prop { name: "column-rule-break", inherited: false, initial: "normal", kind: Kind::Keywords(&["normal", "none", "intersection"]) },
    Prop { name: "row-rule-break", inherited: false, initial: "normal", kind: Kind::Keywords(&["normal", "none", "intersection"]) },
    Prop { name: "rule-overlap", inherited: false, initial: "row-over-column", kind: Kind::Keywords(&["row-over-column", "column-over-row"]) },
    Prop { name: "column-rule-visibility-items", inherited: false, initial: "normal", kind: Kind::Keywords(&["all", "around", "between", "normal"]) },
    Prop { name: "row-rule-visibility-items", inherited: false, initial: "normal", kind: Kind::Keywords(&["all", "around", "between", "normal"]) },
    Prop { name: "hyphens", inherited: true, initial: "manual", kind: Kind::Text },
    Prop { name: "text-wrap-mode", inherited: true, initial: "wrap", kind: Kind::Text },
    Prop { name: "text-wrap-style", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "white-space-collapse", inherited: true, initial: "collapse", kind: Kind::Text },
    Prop { name: "text-autospace", inherited: true, initial: "normal", kind: Kind::Text },
    Prop { name: "text-spacing-trim", inherited: true, initial: "normal", kind: Kind::Text },
    Prop { name: "text-fit", inherited: true, initial: "none", kind: Kind::Text },
    Prop { name: "hyphenate-character", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "word-space-transform", inherited: true, initial: "none", kind: Kind::Text },
    Prop { name: "hanging-punctuation", inherited: true, initial: "none", kind: Kind::Text },
    Prop { name: "text-group-align", inherited: false, initial: "none", kind: Kind::Text },
    Prop { name: "text-align-all", inherited: true, initial: "start", kind: Kind::Text },
    Prop { name: "cursor", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "caret-color", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "accent-color", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "resize", inherited: false, initial: "none", kind: Kind::Text },
    Prop { name: "interactivity", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "caret-animation", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "caret-shape", inherited: true, initial: "auto", kind: Kind::Text },
    Prop { name: "grid-row-start", inherited: false, initial: "auto", kind: Kind::GridLine },
    Prop { name: "grid-column-start", inherited: false, initial: "auto", kind: Kind::GridLine },
    Prop { name: "grid-row-end", inherited: false, initial: "auto", kind: Kind::GridLine },
    Prop { name: "grid-column-end", inherited: false, initial: "auto", kind: Kind::GridLine },
    Prop { name: "corner-top-left-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-top-right-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-bottom-right-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-bottom-left-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-start-start-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-start-end-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-end-start-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "corner-end-end-shape", inherited: false, initial: "superellipse(1)", kind: Kind::Corner },
    Prop { name: "background-position-x", inherited: false, initial: "0%", kind: Kind::BgPos(background_positions::Axis::X) },
    Prop { name: "background-position-y", inherited: false, initial: "0%", kind: Kind::BgPos(background_positions::Axis::Y) },
    Prop { name: "object-view-box", inherited: false, initial: "none", kind: Kind::Shape(shapes::Prop::ObjectViewBox) },
    Prop { name: "border-shape", inherited: false, initial: "none", kind: Kind::Shape(shapes::Prop::BorderShape) },
    Prop { name: "math-style", inherited: true, initial: "normal", kind: Kind::Keywords(&["normal", "compact"]) },
    Prop { name: "math-depth", inherited: true, initial: "0", kind: Kind::MathDepth },
];

pub(crate) const COUNT: usize = PROPS.len() + value_props::COUNT;
pub(crate) const VALUE_BASE: usize = BASE + PROPS.len();

pub(crate) const fn slot_const(name: &str) -> usize {
    let mut index = 0;
    while index < PROPS.len() {
        if same_name(PROPS[index].name, name) {
            return BASE + index;
        }
        index += 1;
    }
    panic!("unknown extra property")
}

const fn same_name(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

fn prop_get(index: usize) -> Option<Prop> {
    if let Some(prop) = PROPS.get(index) {
        return Some(*prop);
    }
    let prop = value_props::PROPS.get(index - PROPS.len())?;
    Some(Prop { name: prop.name, inherited: prop.inherited, initial: prop.initial, kind: Kind::Value(index - PROPS.len()) })
}

fn prop_at(index: usize) -> Prop {
    prop_get(index).expect("extra property index")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values(Option<Arc<[Option<Arc<str>>; COUNT]>>);

impl Values {
    pub const NONE: Self = Self(None);

    fn get(&self, index: usize) -> Option<&Arc<str>> {
        self.0.as_ref().and_then(|values| values[index].as_ref())
    }

    fn set(&mut self, index: usize, value: Option<Arc<str>>) {
        if self.get(index) == value.as_ref() {
            return;
        }
        let values = self.0.get_or_insert_with(|| Arc::new([const { None }; COUNT]));
        Arc::make_mut(values)[index] = value;
    }
}

pub(crate) fn is_extra(slot: usize) -> bool {
    slot >= BASE && slot < BASE + COUNT
}

const fn is_white_space_name(name: &str) -> bool {
    same_name(name, "text-wrap-mode") || same_name(name, "white-space-collapse")
}

const WHITE_SPACE_SLOTS: [usize; 2] = [slot_const("text-wrap-mode"), slot_const("white-space-collapse")];

const INHERITED_COUNT: usize = {
    let (mut index, mut count) = (0, 0);
    while index < COUNT {
        if inherits_by_copy(index) {
            count += 1;
        }
        index += 1;
    }
    count
};

const fn inherits_by_copy(index: usize) -> bool {
    if index < PROPS.len() {
        PROPS[index].inherited && !is_white_space_name(PROPS[index].name)
    } else {
        value_props::PROPS[index - PROPS.len()].inherited && !is_white_space_name(value_props::PROPS[index - PROPS.len()].name)
    }
}

static INHERITED_SLOTS: [usize; INHERITED_COUNT] = {
    let mut slots = [0; INHERITED_COUNT];
    let (mut index, mut next) = (0, 0);
    while index < COUNT {
        if inherits_by_copy(index) {
            slots[next] = BASE + index;
            next += 1;
        }
        index += 1;
    }
    slots
};

pub(crate) fn inherited_slots() -> impl Iterator<Item = usize> {
    INHERITED_SLOTS.iter().copied()
}

pub(crate) fn copy_slot(slot: usize, from: &Style, to: &mut Style) {
    let index = slot - BASE;
    if WHITE_SPACE_SLOTS.contains(&slot) {
        text_props::copy_white_space(prop_at(index).name, from, to);
        return;
    }
    let value = from.generic.get(index).cloned();
    if to.generic.get(index) != value.as_ref() {
        mirror_svg_reference(slot, value.as_deref().unwrap_or(prop_at(index).initial), to);
        to.generic.set(index, value);
        mirror_spacing_trim(slot, to);
        mirror_grid_lines(slot, to);
        mirror_background_position(slot, to);
    }
}

pub(crate) fn inherit(parent: &Style, style: &mut Style) {
    for slot in inherited_slots() {
        copy_slot(slot, parent, style);
    }
}

const NAMES: [&str; COUNT] = {
    let mut names = [""; COUNT];
    let mut index = 0;
    while index < COUNT {
        names[index] = if index < PROPS.len() { PROPS[index].name } else { value_props::PROPS[index - PROPS.len()].name };
        index += 1;
    }
    names
};
static NAME_TABLE: lumen_common::name_table::NameTable<1024> = lumen_common::name_table::NameTable::build(&NAMES);

fn index_of(name: &str) -> Option<usize> {
    let name = if name == "-webkit-text-size-adjust" { "text-size-adjust" } else { name };
    NAME_TABLE.find(&NAMES, name)
}

pub(crate) fn property_index(name: &str) -> Option<usize> {
    index_of(name)
}

pub(crate) fn slot_of(name: &str) -> Option<usize> {
    index_of(name).map(|index| BASE + index)
}

pub(crate) fn is_discrete(name: &str) -> bool {
    index_of(name).is_some_and(|index| matches!(prop_at(index).kind, Kind::Keywords(_) | Kind::ContentVisibility | Kind::Anchor | Kind::MaskBorder(0 | 4) | Kind::Gap(gap_rules::Part::Style) | Kind::GridLine)
        || matches!(prop_at(index).kind, Kind::Value(value) if value_props::is_discrete(value))
        || (prop_at(index).kind == Kind::Text && !text_props::is_color(name)))
}

pub(super) fn lower(raw: &str) -> alloc::borrow::Cow<'_, str> {
    ascii_lower(raw.trim())
}

pub(super) fn zoom_string(value: f64) -> String {
    computed_values::number((if value > 0.0 { value } else { 1.0 }) as f32)
}

pub(super) fn plain_context(context: LengthContext) -> LengthContext {
    LengthContext { percent: None, ..context }
}

pub(super) fn query_for(context: LengthContext) -> ContainerUnitContext {
    ContainerUnitContext::no_container(context.viewport)
}

pub(super) fn parse_number(raw: &str, context: LengthContext) -> Option<f64> {
    let context = plain_context(context);
    css_scalar_with_length_context(raw, false, context, query_for(context)).map(|(value, _)| f64::from(value))
}

pub(super) fn parse_length(raw: &str, context: LengthContext) -> Option<f64> {
    if raw.contains('%') {
        return None;
    }
    let context = plain_context(context);
    contextual_length_with_query(raw, Some(context), true, query_for(context)).map(f64::from)
}

pub(super) fn negative_literal(raw: &str, value: f64) -> bool {
    value < 0.0 && !math_function(raw.trim())
}

fn parse_percentage(raw: &str) -> Option<f64> {
    let value = typed_numeric::parse_numeric_value(raw)?;
    (value.unit == typed_numeric::NumericUnit::Percent).then_some(value.value)
}

pub(super) fn integer(raw: &str, context: LengthContext) -> Option<i64> {
    if !math_function(raw.trim()) {
        let digits = raw.trim().trim_start_matches(['+', '-']);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
    }
    let context = plain_context(context);
    css_integer_with_context(raw, Some(context), query_for(context)).map(i64::from)
}

fn hyphen_chars(raw: &str, context: LengthContext) -> Option<String> {
    let tokens = components(raw)?;
    if tokens.is_empty() || tokens.len() > 3 {
        return None;
    }
    let mut parts: Vec<Option<i64>> = Vec::new();
    for token in &tokens {
        if decoded_css_keyword(token, "auto") {
            parts.push(None);
        } else {
            let value = integer(token, context)?;
            if value < 0 && !math_function(token) {
                return None;
            }
            parts.push(Some(value.max(0)));
        }
    }
    let first = parts[0];
    let second = parts.get(1).copied().unwrap_or(None);
    let third = parts.get(2).copied().unwrap_or(second);
    Some(serialize_hyphen_chars([first, second, third]))
}

fn serialize_hyphen_chars(parts: [Option<i64>; 3]) -> String {
    let show = |value: Option<i64>| value.map_or_else(|| String::from("auto"), |value| value.to_string());
    if parts[2] == parts[1] {
        if parts[1].is_none() {
            show(parts[0])
        } else {
            alloc::format!("{} {}", show(parts[0]), show(parts[1]))
        }
    } else {
        alloc::format!("{} {} {}", show(parts[0]), show(parts[1]), show(parts[2]))
    }
}

fn parse_hyphen_chars(value: &str) -> Option<[Option<i64>; 3]> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let part = |token: &str| -> Option<Option<i64>> {
        if token == "auto" { Some(None) } else { token.parse().ok().map(Some) }
    };
    let first = part(tokens.first()?)?;
    let second = match tokens.get(1) {
        Some(token) => part(token)?,
        None => None,
    };
    let third = match tokens.get(2) {
        Some(token) => part(token)?,
        None => second,
    };
    Some([first, second, third])
}

pub(super) fn compute(slot: usize, raw: &str, context: LengthContext, current: Rgba) -> Option<Arc<str>> {
    let prop = prop_get(slot.checked_sub(BASE)?)?;
    let raw = raw.trim();
    let value = match prop.kind {
        Kind::TabSize => {
            if let Some(value) = parse_number(raw, context) {
                if negative_literal(raw, value) {
                    return None;
                }
                computed_values::number(value.max(0.0) as f32)
            } else {
                let value = parse_length(raw, context)?;
                if negative_literal(raw, value) {
                    return None;
                }
                computed_values::px(value.max(0.0) as f32)
            }
        }
        Kind::TextSizeAdjust => {
            let keyword = lower(raw);
            match keyword.as_ref() {
                "auto" => String::from("auto"),
                "none" => String::from("100%"),
                _ => {
                    let value = parse_percentage(raw)?;
                    if value < 0.0 {
                        return None;
                    }
                    alloc::format!("{}%", computed_values::number(value as f32))
                }
            }
        }
        Kind::Keywords(allowed) => {
            let keyword = lower(raw);
            let keyword = allowed.iter().find(|candidate| **candidate == keyword.as_ref())?;
            if prop.name == "text-justify" && *keyword == "distribute" {
                String::from("inter-character")
            } else {
                String::from(*keyword)
            }
        }
        Kind::Perspective => {
            if decoded_css_keyword(raw, "none") {
                String::from("none")
            } else {
                let value = parse_length(raw, context)?;
                if negative_literal(raw, value) {
                    return None;
                }
                computed_values::px(value.max(0.0) as f32)
            }
        }
        Kind::Position => background_positions::position_compute(raw, context)?,
        Kind::HyphenChars => hyphen_chars(raw, context)?,
        Kind::HyphenLines => {
            if decoded_css_keyword(raw, "no-limit") {
                String::from("no-limit")
            } else {
                let value = integer(raw, context)?;
                if value < 0 && !math_function(raw) {
                    return None;
                }
                value.max(0).to_string()
            }
        }
        Kind::FilterList => {
            if decoded_css_keyword(raw, "none") {
                String::from("none")
            } else {
                let list = filter_source_list(raw, &legacy_source_color(current), context, query_for(context))?;
                computed_values::filters_css_value(Some(&list.filters), |_, color| color_values::serialize(color, false))
            }
        }
        Kind::MaskBorder(component) => {
            let slot = 227 + component;
            let image = border_images::parse_component(slot, raw, context, query_for(context), current)?;
            border_images::serialize(&image, slot)
        }
        Kind::ContentVisibility => {
            let keyword = lower(raw);
            match keyword.as_ref() {
                "visible" | "auto" | "hidden" => keyword.into_owned(),
                _ => return None,
            }
        }
        Kind::Anchor => anchor::computed(prop.name, raw)?,
        Kind::Gap(part) => gap_rules::compute(part, raw, context, current)?,
        Kind::Corner => corner_shapes::compute(raw, context)?,
        Kind::BgPos(axis) => background_positions::compute(axis, raw, context)?,
        Kind::Shape(shape) => shapes::compute(shape, raw, context)?,
        Kind::Text => text_props::compute(prop.name, raw, current)?,
        Kind::GridLine => grid_lines::line(raw)?,
        Kind::MathDepth => math_depth_specified(raw, context)?,
        Kind::Value(index) => value_props::compute(index, raw, context, current)?,
        Kind::Zoom => {
            if decoded_css_keyword(raw, "normal") {
                String::from("1")
            } else {
                let plain = plain_context(context);
                let (value, _) = css_scalar_with_length_context(raw, true, plain, query_for(plain))?;
                let value = f64::from(value);
                if negative_literal(raw, value) {
                    return None;
                }
                zoom_string(value)
            }
        }
    };
    Some(Arc::from(value))
}

fn math_depth_specified(raw: &str, context: LengthContext) -> Option<String> {
    let raw = raw.trim();
    let keyword = lower(raw);
    if keyword == "auto-add" {
        return Some(String::from("auto-add"));
    }
    if keyword.starts_with("add(") && keyword.ends_with(')') {
        return integer(&raw[4..raw.len() - 1], context).map(|value| alloc::format!("add({value})"));
    }
    integer(raw, context).map(|value| value.to_string())
}

fn resolve_math_depth(specified: &str, style: &Style, parent: Option<&Style>) -> Arc<str> {
    let source = parent.unwrap_or(style);
    let inherited = source.generic.get(slot_const("math-depth") - BASE).and_then(|value| value.parse::<i64>().ok()).unwrap_or(0);
    let compact = style.generic.get(slot_const("math-style") - BASE).is_some_and(|value| &**value == "compact");
    let depth = if specified == "auto-add" {
        inherited.saturating_add(i64::from(compact))
    } else if let Some(delta) = specified.strip_prefix("add(").and_then(|rest| rest.strip_suffix(')')) {
        inherited.saturating_add(delta.parse::<i64>().unwrap_or(0))
    } else {
        specified.parse::<i64>().unwrap_or(0)
    };
    Arc::from(depth.to_string())
}

pub(super) fn apply(slot: usize, raw: &str, context: LengthContext, style: &mut Style) {
    apply_with_parent(slot, raw, context, style, None);
}

pub(super) fn apply_with_parent(slot: usize, raw: &str, context: LengthContext, style: &mut Style, parent: Option<&Style>) {
    if let Some(mut value) = compute(slot, raw, context, style.color) {
        if prop_at(slot - BASE).kind == Kind::MathDepth {
            value = resolve_math_depth(&value, style, parent);
        }
        if text_props::apply_white_space(prop_at(slot - BASE).name, &value, style) {
            return;
        }
        mirror_svg_reference(slot, &value, style);
        let initial = prop_at(slot - BASE).initial == &*value;
        let value = if initial { None } else { Some(value) };
        if style.generic.get(slot - BASE) != value.as_ref() {
            style.generic.set(slot - BASE, value);
        }
        mirror_spacing_trim(slot, style);
        mirror_grid_lines(slot, style);
        mirror_background_position(slot, style);
    }
}

fn mirror_spacing_trim(slot: usize, style: &mut Style) {
    if prop_get(slot - BASE).is_some_and(|prop| prop.name == "text-spacing-trim") {
        let trim = crate::paint::SpacingTrim::from_keyword(style.generic.get(slot - BASE).map_or("normal", |value| &**value));
        if style.font.spacing_trim != trim {
            style.font.spacing_trim = trim;
        }
    }
}

fn mirror_background_position(slot: usize, style: &mut Style) {
    if prop_get(slot - BASE).is_some_and(|prop| matches!(prop.kind, Kind::BgPos(_))) {
        background_positions::mirror(style);
    }
}

fn mirror_grid_lines(slot: usize, style: &mut Style) {
    if prop_get(slot - BASE).is_some_and(|prop| prop.kind == Kind::GridLine) {
        grid_lines::mirror(style);
    }
}

fn mirror_svg_reference(slot: usize, value: &str, style: &mut Style) {
    if prop_get(slot - BASE).is_some_and(|prop| prop.kind == Kind::Shape(shapes::Prop::ClipPath)) {
        style.svg_clip_path = shapes::svg_reference(value);
    }
}

pub(crate) fn compose(name: &str, base: &str, value: &str, accumulate: bool) -> Option<String> {
    if name == "filter" {
        return filter_lists::compose(base, value, accumulate);
    }
    match prop_at(index_of(name)?).kind {
        Kind::FilterList => filter_lists::compose(base, value, accumulate),
        Kind::Shape(shape) => {
            if accumulate { shapes::accumulate(shape, base, value, 1.0) } else { shapes::add(shape, base, value) }
        }
        Kind::MaskBorder(component @ 1..=3) => {
            let (a, b) = mask_border_pair(component, base, value)?;
            crate::animation::transition_values::compose_border_image(MASK_BORDER_PROXY[component - 1], &a, &b)
        }
        Kind::Text if text_props::is_color(name) => text_props::compose_color(base, value),
        Kind::BgPos(axis) => background_positions::compose(axis, base, value),
        Kind::Position => background_positions::compose_position(base, value),
        Kind::Value(_) if name == "font-variation-settings" => font_variations::compose(base, value),
        Kind::Value(_) if name == "text-decoration-inset" => text_decoration_inset::compose(base, value),
        _ => None,
    }
}

/// The specified keyword of a keyword-valued property, or `None` for its initial value.
pub(crate) fn keyword<'a>(style: &'a Style, name: &str) -> Option<&'a str> {
    style.generic.get(index_of(name)?).map(|value| &**value)
}

pub(crate) fn set_stored(style: &mut Style, name: &str, value: Option<&str>) {
    if let Some(index) = index_of(name) {
        style.generic.set(index, value.map(Arc::from));
    }
}

pub(crate) fn stored(style: &Style, name: &str) -> Option<String> {
    let index = index_of(name)?;
    Some(style.generic.get(index).map_or_else(|| String::from(prop_at(index).initial), |value| value.to_string()))
}

pub(crate) fn computed(style: &Style, name: &str) -> Option<String> {
    if let Some(index) = index_of(name) {
        return computed_longhand(style, name, index);
    }
    if corner_radii::is_shorthand(name) {
        return Some(corner_radii::computed_shorthand(style, name));
    }
    if background_positions::is_shorthand(name) {
        return Some(background_positions::computed_shorthand(style));
    }
    if grid_lines::is_shorthand(name) {
        return Some(grid_lines::computed_shorthand(style, name));
    }
    if gap_rules::is_shorthand(name) {
        return Some(gap_rules::computed_shorthand(style, name));
    }
    if corner_shapes::is_shorthand(name) {
        return Some(corner_shapes::computed_shorthand(style, name));
    }
    if value_shorthands::is_shorthand(name) {
        return Some(value_shorthands::computed_shorthand(style, name));
    }
    if text_props::is_shorthand(name) {
        let values: Option<Vec<String>> = text_props::longhands(name).iter().map(|longhand| computed(style, longhand)).collect();
        let values = values?;
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        return text_props::shorthand_value(name, &refs).or_else(|| Some(String::new()));
    }
    None
}

fn computed_longhand(style: &Style, name: &str, index: usize) -> Option<String> {
    if let Kind::Gap(part) = prop_at(index).kind {
        return Some(gap_rules::computed(style, name, part));
    }
    if name == "object-position" {
        let stored = stored(style, name)?;
        if background_positions::has_logical_word(&stored) {
            return background_positions::position_physical(&stored, style.writing_mode, style.direction).or(Some(stored));
        }
        return Some(stored);
    }
    if prop_at(index).kind == Kind::Text {
        let stored = stored(style, name)?;
        return Some(text_props::computed(name, &stored, style));
    }
    stored(style, name)
}

pub(super) fn declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    if grid_lines::is_shorthand(name) {
        return grid_lines::shorthand_declarations(name, raw);
    }
    if gap_rules::is_shorthand(name) {
        return gap_rules::shorthand_declarations(name, raw);
    }
    if corner_shapes::is_shorthand(name) {
        return corner_shapes::shorthand_declarations(name, raw);
    }
    if background_positions::is_shorthand(name) {
        return background_positions::shorthand_declarations(name, raw);
    }
    if value_shorthands::is_shorthand(name) {
        return value_shorthands::shorthand_declarations(name, raw);
    }
    if text_props::is_shorthand(name) {
        let mut values = Vec::new();
        for (longhand, text) in text_props::expand(name, raw)? {
            values.extend(declarations(longhand, &text)?);
        }
        return Some(values);
    }
    let index = index_of(name)?;
    let slot = BASE + index;
    compute(slot, raw, static_length_context(), Style::initial().color)?;
    let value = alloc::vec![Value::ExtraRaw(slot, Arc::from(raw.trim()))];
    Some(match prop_at(index).kind {
        Kind::Gap(_) => gap_rules::longhand_declarations(name, raw, value),
        _ => value,
    })
}

pub(crate) fn serialize_specified(slot: usize, raw: &str) -> Option<String> {
    let prop = prop_get(slot.checked_sub(BASE)?)?;
    let raw = raw.trim();
    match prop.kind {
        Kind::TabSize | Kind::Perspective if !length_independent(raw) || math_function(raw) => {
            typed_numeric::parse_numeric_expression(raw)?.serialize_specified()
        }
        Kind::TabSize | Kind::Perspective if typed_numeric::parse_numeric_expression(raw).is_some() => {
            typed_numeric::parse_numeric_expression(raw)?.serialize_specified()
        }
        Kind::TextSizeAdjust if math_function(raw) => typed_numeric::parse_numeric_expression(raw)?.serialize_specified(),
        Kind::Position => background_positions::position_specified(raw),
        Kind::Anchor => anchor::specified(prop.name, raw),
        Kind::Gap(part) => gap_rules::specified(part, raw),
        Kind::Corner => corner_shapes::specified(raw),
        Kind::BgPos(axis) => background_positions::specified(axis, raw),
        Kind::Shape(shape) => shapes::specified(shape, raw),
        Kind::Value(index) => value_props::serialize_specified(index, raw),
        Kind::Zoom if decoded_css_keyword(raw, "normal") => Some(String::from("normal")),
        Kind::Zoom => typed_numeric::parse_numeric_expression(raw)?.serialize_specified(),
        Kind::MaskBorder(component @ 0..=3) => border_images::serialize_specified(227 + component, raw),
        _ => compute(slot, raw, static_length_context(), Style::initial().color).map(|value| value.to_string()),
    }
}

pub(super) use lumen_common::numeric::lerp as mix;

pub(super) fn mix_lp(a: &str, b: &str, progress: f64) -> Option<String> {
    let context = static_length_context();
    let query = query_for(context);
    let a = transform_length_with_query(a, context, query)?;
    let b = transform_length_with_query(b, context, query)?;
    Some(computed_values::percentage(
        mix(f64::from(a.pixels), f64::from(b.pixels), progress) as f32,
        mix(f64::from(a.percent), f64::from(b.percent), progress) as f32,
    ))
}

const MASK_BORDER_PROXY: [&str; 3] = ["border-image-slice", "border-image-width", "border-image-outset"];

fn mask_border_pair(component: usize, from: &str, to: &str) -> Option<(BorderImage, BorderImage)> {
    let context = static_length_context();
    let query = query_for(context);
    let color = Style::initial().color;
    let a = border_images::parse_component(227 + component, from, context, query, color)?;
    let b = border_images::parse_component(227 + component, to, context, query, color)?;
    Some((a, b))
}

pub(crate) fn animates_here(name: &str) -> bool {
    name == "filter" || index_of(name).is_some()
}

pub(crate) fn interpolate(name: &str, from: &str, to: &str, progress: f64) -> Option<String> {
    if name == "filter" {
        return filter_lists::interpolate(from, to, progress);
    }
    let index = index_of(name)?;
    if !progress.is_finite() {
        return None;
    }
    if name == "overlay" {
        return Some(if from == to || progress <= 0.0 { from } else if progress >= 1.0 { to } else { "auto" }.to_owned());
    }
    match prop_at(index).kind {
        Kind::TabSize => {
            let (a, unit_a) = syntax::split_number_unit(from)?;
            let (b, unit_b) = syntax::split_number_unit(to)?;
            if unit_a != unit_b {
                return None;
            }
            let value = mix(a, b, progress).max(0.0);
            Some(if unit_a.is_empty() { computed_values::number(value as f32) } else { computed_values::px(value as f32) })
        }
        Kind::TextSizeAdjust => {
            let (a, unit_a) = syntax::split_number_unit(from)?;
            let (b, unit_b) = syntax::split_number_unit(to)?;
            if unit_a != "%" || unit_b != "%" {
                return None;
            }
            Some(alloc::format!("{}%", computed_values::number(mix(a, b, progress).max(0.0) as f32)))
        }
        Kind::Perspective => {
            let (a, _) = syntax::split_number_unit(from).filter(|(_, unit)| *unit == "px")?;
            let (b, _) = syntax::split_number_unit(to).filter(|(_, unit)| *unit == "px")?;
            Some(computed_values::px(mix(a, b, progress).max(0.0) as f32))
        }
        Kind::Position => {
            let (from, to) = (background_positions::position_endpoint(from)?, background_positions::position_endpoint(to)?);
            let (from, to) = (from.as_str(), to.as_str());
            let tokens = components(from)?;
            let (from_x, from_y) = (*tokens.first()?, *tokens.get(1)?);
            let tokens = components(to)?;
            let (to_x, to_y) = (*tokens.first()?, *tokens.get(1)?);
            Some(alloc::format!("{} {}", mix_lp(from_x, to_x, progress)?, mix_lp(from_y, to_y, progress)?))
        }
        Kind::HyphenChars => {
            let a = parse_hyphen_chars(from)?;
            let b = parse_hyphen_chars(to)?;
            let mut out = [None; 3];
            for slot in 0..3 {
                out[slot] = match (a[slot], b[slot]) {
                    (None, None) => None,
                    (Some(x), Some(y)) => Some(libm::floor(mix(x as f64, y as f64, progress) + 0.5) as i64),
                    _ => return None,
                };
            }
            Some(serialize_hyphen_chars(out))
        }
        Kind::HyphenLines => {
            let a: i64 = from.parse().ok()?;
            let b: i64 = to.parse().ok()?;
            Some((libm::floor(mix(a as f64, b as f64, progress) + 0.5) as i64).max(0).to_string())
        }
        Kind::ContentVisibility => match (from, to) {
            ("visible", "hidden") | ("hidden", "visible") => Some(
                if progress <= 0.0 {
                    from
                } else if progress >= 1.0 {
                    to
                } else {
                    "visible"
                }
                .to_owned(),
            ),
            _ => None,
        },
        Kind::FilterList => filter_lists::interpolate(from, to, progress),
        Kind::MaskBorder(component @ 1..=3) => {
            let (a, b) = mask_border_pair(component, from, to)?;
            crate::animation::transition_values::interpolate_border_image(MASK_BORDER_PROXY[component - 1], &a, &b, progress)
        }
        Kind::Zoom => {
            let (a, _) = syntax::split_number_unit(from)?;
            let (b, _) = syntax::split_number_unit(to)?;
            Some(zoom_string(mix(a, b, progress)))
        }
        Kind::Shape(shape) => shapes::interpolate(shape, from, to, progress),
        Kind::Gap(part) => gap_rules::interpolate_snapping(part, from, to, progress, name.starts_with("row-") || from.contains(',') || to.contains(',')),
        Kind::Corner => corner_shapes::interpolate(from, to, progress),
        Kind::BgPos(axis) => background_positions::interpolate(axis, from, to, progress),
        Kind::Text if text_props::is_color(name) => text_props::interpolate_color(from, to, progress),
        Kind::Value(value) => value_props::interpolate(value, from, to, progress),
        Kind::Text | Kind::MaskBorder(_) | Kind::Keywords(_) | Kind::Anchor | Kind::GridLine | Kind::MathDepth => None,
    }
}

pub(crate) fn is_shorthand(name: &str) -> bool {
    gap_rules::is_shorthand(name) || corner_shapes::is_shorthand(name) || value_shorthands::is_shorthand(name) || background_positions::is_shorthand(name)
}

pub(crate) fn shorthand_longhands(name: &str) -> &'static [&'static str] {
    if gap_rules::is_shorthand(name) {
        gap_rules::longhands(name)
    } else if value_shorthands::is_shorthand(name) {
        value_shorthands::longhands(name)
    } else if background_positions::is_shorthand(name) {
        background_positions::longhands(name)
    } else {
        corner_shapes::longhands(name)
    }
}

pub(crate) fn expand_shorthand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    gap_rules::expand(name, raw).or_else(|| corner_shapes::expand(name, raw)).or_else(|| value_shorthands::expand(name, raw)).or_else(|| background_positions::expand(name, raw))
}

pub(crate) fn synthesize_shorthand(name: &str, values: &[&str]) -> Option<String> {
    if gap_rules::is_shorthand(name) {
        gap_rules::synthesize(name, values)
    } else if value_shorthands::is_shorthand(name) {
        value_shorthands::synthesize(name, values)
    } else if background_positions::is_shorthand(name) {
        background_positions::synthesize(name, values)
    } else {
        corner_shapes::synthesize(name, values)
    }
}
