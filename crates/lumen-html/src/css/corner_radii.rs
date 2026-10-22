//! Side and logical-edge radius shorthands such as `border-top-radius`, which
//! set the two corner radii along one edge with the `border-radius` grammar.
use super::*;

struct Short {
    name: &'static str,
    longhands: [&'static str; 2],
}

static TABLE: &[Short] = &[
    Short { name: "border-top-radius", longhands: ["border-top-left-radius", "border-top-right-radius"] },
    Short { name: "border-right-radius", longhands: ["border-top-right-radius", "border-bottom-right-radius"] },
    Short { name: "border-bottom-radius", longhands: ["border-bottom-left-radius", "border-bottom-right-radius"] },
    Short { name: "border-left-radius", longhands: ["border-top-left-radius", "border-bottom-left-radius"] },
    Short { name: "border-block-start-radius", longhands: ["border-start-start-radius", "border-start-end-radius"] },
    Short { name: "border-block-end-radius", longhands: ["border-end-start-radius", "border-end-end-radius"] },
    Short { name: "border-inline-start-radius", longhands: ["border-start-start-radius", "border-end-start-radius"] },
    Short { name: "border-inline-end-radius", longhands: ["border-start-end-radius", "border-end-end-radius"] },
];

pub(crate) fn alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "-webkit-border-radius" => "border-radius",
        "-webkit-border-top-left-radius" => "border-top-left-radius",
        "-webkit-border-top-right-radius" => "border-top-right-radius",
        "-webkit-border-bottom-right-radius" => "border-bottom-right-radius",
        "-webkit-border-bottom-left-radius" => "border-bottom-left-radius",
        _ => return None,
    })
}

fn short(name: &str) -> Option<&'static Short> {
    TABLE.iter().find(|short| short.name == name)
}

pub(crate) fn is_shorthand(name: &str) -> bool {
    short(name).is_some()
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    short(name).map_or(&[], |short| &short.longhands)
}

fn radii<'a>(part: &'a str) -> Option<[&'a str; 2]> {
    match components(part)?.as_slice() {
        [only] => Some([only, only]),
        [first, second] => Some([first, second]),
        _ => None,
    }
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let short = short(name)?;
    let parts = top_level_split(raw.trim(), b'/', 3)?;
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let horizontal = radii(parts[0])?;
    let vertical = match parts.get(1) {
        Some(part) => radii(part)?,
        None => horizontal,
    };
    Some(
        short
            .longhands
            .iter()
            .enumerate()
            .map(|(index, longhand)| {
                let (h, v) = (horizontal[index], vertical[index]);
                (*longhand, if h == v { String::from(h) } else { alloc::format!("{h} {v}") })
            })
            .collect(),
    )
}

pub(crate) fn declarations(name: &str, raw: &str, offset: usize) -> Result<Vec<Declaration>, CssError> {
    let mut out = Vec::new();
    let Some(corners) = expand(name, raw) else { return Ok(out) };
    for (longhand, text) in corners {
        let declarations = typed_declarations(longhand, &text, offset)?;
        if declarations.is_empty() {
            return Ok(Vec::new());
        }
        out.extend(declarations);
    }
    Ok(out)
}

/// `border-radius` serialization compresses the horizontal and vertical
/// side lists independently and drops a vertical list equal to the horizontal.
pub(crate) fn specified_border_radius(raw: &str) -> Option<String> {
    let parts = top_level_split(raw.trim(), b'/', 3)?;
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let sides = |part: &str| -> Option<[String; 4]> {
        let tokens = components(part)?;
        let at = |index: usize| tokens[index].to_string();
        match tokens.len() {
            1 => Some([at(0), at(0), at(0), at(0)]),
            2 => Some([at(0), at(1), at(0), at(1)]),
            3 => Some([at(0), at(1), at(2), at(1)]),
            4 => Some([at(0), at(1), at(2), at(3)]),
            _ => None,
        }
    };
    let horizontal = sides(parts[0])?;
    let vertical = match parts.get(1) {
        Some(part) => sides(part)?,
        None => horizontal.clone(),
    };
    let text = |sides: &[String; 4]| declaration_block::serialize_box_sides(sides.each_ref().map(String::as_str));
    Some(if horizontal == vertical { text(&horizontal) } else { alloc::format!("{} / {}", text(&horizontal), text(&vertical)) })
}

fn split(value: &str) -> Option<[String; 2]> {
    match components(value)?.as_slice() {
        [only] => Some([only.to_string(), only.to_string()]),
        [horizontal, vertical] => Some([horizontal.to_string(), vertical.to_string()]),
        _ => None,
    }
}

fn pair(values: &[String; 2]) -> String {
    if values[0] == values[1] { values[0].clone() } else { alloc::format!("{} {}", values[0], values[1]) }
}

pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    short(name)?;
    let &[first, second] = values else { return None };
    let (first, second) = (split(first)?, split(second)?);
    let horizontal = [first[0].clone(), second[0].clone()];
    let vertical = [first[1].clone(), second[1].clone()];
    Some(if horizontal == vertical {
        pair(&horizontal)
    } else {
        alloc::format!("{} / {}", pair(&horizontal), pair(&vertical))
    })
}

pub(crate) fn computed_shorthand(style: &Style, name: &str) -> String {
    let context = computed_values::ComputedValueContext::default();
    let values: Option<Vec<String>> = longhands(name).iter().map(|longhand| style.computed_css_value(longhand, context)).collect();
    let Some(values) = values else { return String::new() };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    synthesize(name, &refs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_radii_expand_and_compress() {
        let corners = expand("border-top-radius", "1px 2% / 3px").unwrap();
        assert_eq!(corners[0], ("border-top-left-radius", String::from("1px 3px")));
        assert_eq!(corners[1], ("border-top-right-radius", String::from("2% 3px")));
        assert_eq!(synthesize("border-top-radius", &["1px 3px", "2% 3px"]).as_deref(), Some("1px 2% / 3px"));
        assert_eq!(synthesize("border-top-radius", &["10px", "10px"]).as_deref(), Some("10px"));
        assert!(expand("border-top-radius", "1px / 2px 3px 4px").is_none());
    }
}
