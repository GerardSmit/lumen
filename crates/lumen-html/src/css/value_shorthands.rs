//! Shorthands over the grammar-defined longhands in `value_props`.
use super::*;
use super::extra_props as extra;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Sides,
    Pair,
    Repeat,
    NameSlashType,
    Rule,
}

struct Short {
    name: &'static str,
    longhands: &'static [&'static str],
    shape: Shape,
}

static TABLE: &[Short] = &[
    Short {
        name: "scroll-margin",
        longhands: &["scroll-margin-top", "scroll-margin-right", "scroll-margin-bottom", "scroll-margin-left"],
        shape: Shape::Sides,
    },
    Short { name: "scroll-margin-block", longhands: &["scroll-margin-block-start", "scroll-margin-block-end"], shape: Shape::Pair },
    Short { name: "scroll-margin-inline", longhands: &["scroll-margin-inline-start", "scroll-margin-inline-end"], shape: Shape::Pair },
    Short {
        name: "scroll-padding",
        longhands: &["scroll-padding-top", "scroll-padding-right", "scroll-padding-bottom", "scroll-padding-left"],
        shape: Shape::Sides,
    },
    Short { name: "scroll-padding-block", longhands: &["scroll-padding-block-start", "scroll-padding-block-end"], shape: Shape::Pair },
    Short { name: "scroll-padding-inline", longhands: &["scroll-padding-inline-start", "scroll-padding-inline-end"], shape: Shape::Pair },
    Short { name: "overscroll-behavior", longhands: &["overscroll-behavior-x", "overscroll-behavior-y"], shape: Shape::Pair },
    Short { name: "interest-delay", longhands: &["interest-delay-start", "interest-delay-end"], shape: Shape::Pair },
    Short { name: "container", longhands: &["container-name", "container-type"], shape: Shape::NameSlashType },
    Short { name: "text-box", longhands: &["text-box-trim", "text-box-edge"], shape: Shape::Rule },
    Short { name: "text-emphasis", longhands: &["text-emphasis-style", "text-emphasis-color"], shape: Shape::Rule },
    Short { name: "vertical-align", longhands: &["baseline-source", "alignment-baseline", "baseline-shift"], shape: Shape::Rule },
    Short { name: "-webkit-text-stroke", longhands: &["-webkit-text-stroke-width", "-webkit-text-stroke-color"], shape: Shape::Rule },
    Short {
        name: "font-synthesis",
        longhands: &["font-synthesis-weight", "font-synthesis-style", "font-synthesis-small-caps", "font-synthesis-position"],
        shape: Shape::Rule,
    },
    Short { name: "line-clamp", longhands: &["max-lines", "block-ellipsis", "continue"], shape: Shape::Rule },
    Short { name: "-webkit-line-clamp", longhands: &["max-lines", "block-ellipsis", "continue"], shape: Shape::Rule },
    Short { name: "marker", longhands: &["marker-start", "marker-mid", "marker-end"], shape: Shape::Repeat },
];

fn short(name: &str) -> Option<&'static Short> {
    TABLE.iter().find(|short| short.name == name)
}

pub(crate) fn is_shorthand(name: &str) -> bool {
    short(name).is_some()
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    short(name).map_or(&[], |short| short.longhands)
}

fn expand_values(name: &str, shape: Shape, arity: usize, raw: &str) -> Option<Vec<String>> {
    if shape == Shape::Rule {
        return shorthand_rules::expand(name, raw, arity);
    }
    if shape == Shape::NameSlashType {
        let (name, kind) = match raw.split_once('/') {
            Some((name, kind)) => (name.trim(), kind.trim()),
            None => (raw, "normal"),
        };
        if name.is_empty() || kind.is_empty() || kind.contains('/') {
            return None;
        }
        return Some(alloc::vec![String::from(name), String::from(kind)]);
    }
    let tokens = components(raw)?;
    let limit = if shape == Shape::Repeat { 1 } else { arity };
    if tokens.is_empty() || tokens.len() > limit {
        return None;
    }
    let values: Vec<String> = tokens.iter().map(|token| String::from(*token)).collect();
    let at = |index: usize| values[index].clone();
    Some(match (arity, values.len()) {
        (_, 1) => alloc::vec![at(0); arity],
        (2, _) => values,
        (_, 2) => alloc::vec![at(0), at(1), at(0), at(1)],
        (_, 3) => alloc::vec![at(0), at(1), at(2), at(1)],
        _ => values,
    })
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let short = short(name)?;
    let values = expand_values(name, short.shape, short.longhands.len(), raw.trim())?;
    Some(short.longhands.iter().copied().zip(values).collect())
}

pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    let short = short(name)?;
    if values.len() != short.longhands.len() {
        return None;
    }
    match short.shape {
        Shape::Rule => shorthand_rules::synthesize(name, values, false),
        Shape::Sides => {
            let &[a, b, c, d] = values else { return None };
            Some(declaration_block::serialize_box_sides([a, b, c, d]))
        }
        Shape::Pair => {
            let &[a, b] = values else { return None };
            Some(if a == b { String::from(a) } else { alloc::format!("{a} {b}") })
        }
        Shape::NameSlashType => {
            let &[name, kind] = values else { return None };
            Some(if kind == "normal" { String::from(name) } else { alloc::format!("{name} / {kind}") })
        }
        Shape::Repeat => {
            let first = values.first()?;
            values.iter().all(|value| value == first).then(|| String::from(*first))
        }
    }
}

pub(crate) fn computed_shorthand(style: &Style, name: &str) -> String {
    let values: Option<Vec<String>> = longhands(name)
        .iter()
        .map(|longhand| match *longhand {
            "container-type" => Some(style.container_type.serialize(style.container_scroll_state)),
            "font-synthesis-small-caps" => Some(String::from(if style.font.synthesize_small_caps { "auto" } else { "none" })),
            _ => extra::computed(style, longhand),
        })
        .collect();
    let Some(values) = values else { return String::new() };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    if short(name).is_some_and(|short| short.shape == Shape::Rule) {
        return shorthand_rules::synthesize(name, &refs, true).unwrap_or_default();
    }
    synthesize(name, &refs).unwrap_or_default()
}

pub(crate) fn shorthand_declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    let expanded = expand(name, raw)?;
    if name == "vertical-align" {
        values.push(Value::VerticalAlign(legacy_vertical_align(&expanded)));
    }
    for (longhand, text) in expanded {
        if longhand == "container-type" {
            let (axis, scroll_state) = ContainerType::parse(&text)?;
            values.push(Value::ContainerType(axis, scroll_state));
        } else if longhand == "font-synthesis-small-caps" {
            values.push(Value::FontSynthesisSmallCaps(text != "none"));
        } else {
            values.extend(extra::declarations(longhand, &text)?);
        }
    }
    Some(values)
}

fn legacy_vertical_align(expanded: &[(&'static str, String)]) -> VerticalAlign {
    let text = |name: &str| expanded.iter().find(|(longhand, _)| *longhand == name).map_or("", |(_, value)| value.as_str());
    let shift = ascii_lower(text("baseline-shift"));
    match shift.as_ref() {
        "sub" => return VerticalAlign::Sub,
        "super" => return VerticalAlign::Super,
        "top" => return VerticalAlign::Top,
        "bottom" => return VerticalAlign::Bottom,
        "center" => return VerticalAlign::Middle,
        _ => {}
    }
    if let Some(length) = background_length(&shift, static_length_context()) {
        if length != LengthPercentage::default() {
            return VerticalAlign::Length(length);
        }
    }
    match text("alignment-baseline") {
        "middle" => VerticalAlign::Middle,
        "text-top" => VerticalAlign::TextTop,
        "text-bottom" => VerticalAlign::TextBottom,
        _ => VerticalAlign::Baseline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sides_expand_in_box_order() {
        let expanded = expand("scroll-margin", "1px 2px 3px").unwrap();
        let values: Vec<&str> = expanded.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(values, ["1px", "2px", "3px", "2px"]);
        assert_eq!(synthesize("scroll-margin", &["1px", "2px", "1px", "2px"]).as_deref(), Some("1px 2px"));
    }

    #[test]
    fn marker_repeats_one_value() {
        assert!(expand("marker", "none none").is_none());
        assert_eq!(synthesize("marker", &["none", "none", "url(a)"]), None);
    }
}
