//! Shorthands whose longhand values depend on which components the author omitted, so a
//! positional expansion cannot describe them.
use super::*;
use super::value_props;

const WIDE: [&str; 5] = ["initial", "inherit", "unset", "revert", "revert-layer"];

fn wide_keyword(token: &str) -> Option<&'static str> {
    WIDE.into_iter().find(|keyword| decoded_css_keyword(token, keyword))
}

fn specified(longhand: &str, raw: &str) -> Option<String> {
    value_props::serialize_specified(value_props::index_of(longhand)?, raw)
}

pub(super) fn expand(name: &str, raw: &str, arity: usize) -> Option<Vec<String>> {
    let tokens = components(raw)?;
    if tokens.len() == 1 {
        if let Some(keyword) = wide_keyword(tokens[0]) {
            return Some(alloc::vec![String::from(keyword); arity]);
        }
    }
    match name {
        "text-box" => text_box(&tokens),
        "text-emphasis" => text_emphasis(&tokens),
        "-webkit-text-stroke" => text_stroke(&tokens),
        "vertical-align" => vertical_align(&tokens),
        "font-synthesis" => font_synthesis(&tokens),
        _ => None,
    }
}

fn text_box(tokens: &[&str]) -> Option<Vec<String>> {
    if tokens.len() == 1 && decoded_css_keyword(tokens[0], "normal") {
        return Some(alloc::vec![String::from("none"), String::from("auto")]);
    }
    let trim_of = |token: &str| ["none", "trim-start", "trim-end", "trim-both"].into_iter().find(|keyword| decoded_css_keyword(token, keyword));
    let first = trim_of(tokens.first()?);
    let last = trim_of(tokens.last()?);
    let (trim, edge) = match (first, last) {
        (Some(trim), _) => (Some(trim), &tokens[1..]),
        (None, Some(trim)) => (Some(trim), &tokens[..tokens.len() - 1]),
        (None, None) => (None, tokens),
    };
    let edge = if edge.is_empty() { String::from("auto") } else { specified("text-box-edge", &edge.join(" "))? };
    let trim = trim.unwrap_or("trim-both");
    Some(alloc::vec![String::from(trim), edge])
}

fn text_emphasis(tokens: &[&str]) -> Option<Vec<String>> {
    let is_style = |token: &str| {
        token.starts_with(['"', '\''])
            || ["none", "filled", "open", "dot", "circle", "double-circle", "triangle", "sesame"].into_iter().any(|keyword| decoded_css_keyword(token, keyword))
    };
    let styled: Vec<usize> = (0..tokens.len()).filter(|at| is_style(tokens[*at])).collect();
    let colored: Vec<usize> = (0..tokens.len()).filter(|at| !is_style(tokens[*at])).collect();
    if colored.len() > 1 || styled.windows(2).any(|pair| pair[1] != pair[0] + 1) {
        return None;
    }
    let style = if styled.is_empty() { String::from("none") } else {
        specified("text-emphasis-style", &styled.iter().map(|at| tokens[*at]).collect::<Vec<_>>().join(" "))?
    };
    let color = match colored.first() {
        Some(at) => specified("text-emphasis-color", tokens[*at])?,
        None => String::from("currentcolor"),
    };
    Some(alloc::vec![style, color])
}

fn vertical_align(tokens: &[&str]) -> Option<Vec<String>> {
    const LONGHANDS: [&str; 3] = ["baseline-source", "alignment-baseline", "baseline-shift"];
    if tokens.is_empty() || tokens.len() > 3 {
        return None;
    }
    let mut parts: [Option<String>; 3] = [None, None, None];
    for token in tokens {
        let lower = ascii_lower(token);
        let index = match lower.as_ref() {
            "first" | "last" => 0,
            "baseline" | "text-bottom" | "alphabetic" | "ideographic" | "middle" | "central" | "mathematical" | "text-top" => 1,
            _ => 2,
        };
        if parts[index].is_some() {
            return None;
        }
        parts[index] = Some(specified(LONGHANDS[index], token)?);
    }
    let [source, alignment, shift] = parts;
    Some(alloc::vec![
        source.unwrap_or_else(|| String::from("auto")),
        alignment.unwrap_or_else(|| String::from("baseline")),
        shift.unwrap_or_else(|| String::from("0px")),
    ])
}

fn text_stroke(tokens: &[&str]) -> Option<Vec<String>> {
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    let (mut width, mut color) = (None, None);
    for token in tokens {
        if width.is_none() {
            if let Some(value) = specified("-webkit-text-stroke-width", token) {
                width = Some(value);
                continue;
            }
        }
        if color.is_some() {
            return None;
        }
        color = Some(specified("-webkit-text-stroke-color", token)?);
    }
    Some(alloc::vec![width.unwrap_or_else(|| String::from("0px")), color.unwrap_or_else(|| String::from("currentcolor"))])
}

fn font_synthesis(tokens: &[&str]) -> Option<Vec<String>> {
    if tokens.len() == 1 && decoded_css_keyword(tokens[0], "none") {
        return Some(alloc::vec![String::from("none"); 4]);
    }
    let (mut weight, mut style, mut small_caps, mut position) = ("none", "none", "none", "none");
    let mut seen = [false; 4];
    for token in tokens {
        let (slot, target, value) = if decoded_css_keyword(token, "weight") {
            (0, &mut weight, "auto")
        } else if decoded_css_keyword(token, "style") {
            (1, &mut style, "auto")
        } else if decoded_css_keyword(token, "oblique-only") {
            (1, &mut style, "oblique-only")
        } else if decoded_css_keyword(token, "small-caps") {
            (2, &mut small_caps, "auto")
        } else if decoded_css_keyword(token, "position") {
            (3, &mut position, "auto")
        } else {
            return None;
        };
        if seen[slot] {
            return None;
        }
        seen[slot] = true;
        *target = value;
    }
    Some([weight, style, small_caps, position].into_iter().map(String::from).collect())
}

pub(super) fn synthesize(name: &str, values: &[&str], computed: bool) -> Option<String> {
    if let Some(first) = values.first().filter(|first| values.iter().all(|value| value == *first)) {
        if let Some(keyword) = wide_keyword(first) {
            return Some(String::from(keyword));
        }
    }
    match name {
        "text-box" => {
            let &[trim, edge] = values else { return None };
            Some(match (trim, edge) {
                ("none", "auto") => String::from("normal"),
                ("trim-both", "auto") => String::from("trim-both"),
                ("trim-both", edge) => String::from(edge),
                (trim, "auto") => String::from(trim),
                (trim, edge) => alloc::format!("{trim} {edge}"),
            })
        }
        "vertical-align" => {
            let &[source, alignment, shift] = values else { return None };
            let parts: Vec<&str> = [(source, "auto"), (alignment, "baseline"), (shift, "0px")].into_iter()
                .filter(|(value, initial)| value != initial).map(|(value, _)| value).collect();
            Some(if parts.is_empty() { String::from("baseline") } else { parts.join(" ") })
        }
        "-webkit-text-stroke" => {
            let &[width, color] = values else { return None };
            if computed {
                return Some(alloc::format!("{width} {color}"));
            }
            let parts: Vec<&str> = [(width, "0px"), (color, "currentcolor")].into_iter()
                .filter(|(value, initial)| value != initial).map(|(value, _)| value).collect();
            Some(if parts.is_empty() { String::from("0px") } else { parts.join(" ") })
        }
        "text-emphasis" => {
            let &[style, color] = values else { return None };
            if computed {
                return Some(alloc::format!("{style} {color}"));
            }
            let parts: Vec<&str> = [(style, "none"), (color, "currentcolor")].into_iter()
                .filter(|(value, initial)| value != initial).map(|(value, _)| value).collect();
            Some(if parts.is_empty() { String::from("none") } else { parts.join(" ") })
        }
        "font-synthesis" => {
            let &[weight, style, small_caps, position] = values else { return None };
            let mut parts: Vec<&str> = Vec::new();
            if weight == "auto" {
                parts.push("weight");
            }
            match style {
                "auto" => parts.push("style"),
                "oblique-only" => parts.push("oblique-only"),
                _ => {}
            }
            if small_caps == "auto" {
                parts.push("small-caps");
            }
            if position == "auto" {
                parts.push("position");
            }
            Some(if parts.is_empty() { String::from("none") } else { parts.join(" ") })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(name: &str, raw: &str) -> Option<Vec<String>> {
        expand(name, raw, if name == "font-synthesis" { 4 } else { 2 })
    }

    #[test]
    fn text_box_defaults_trim_when_only_an_edge_is_given() {
        assert_eq!(expanded("text-box", "cap text trim-end").unwrap(), ["trim-end", "cap"]);
        assert_eq!(expanded("text-box", "auto").unwrap(), ["trim-both", "auto"]);
        assert!(expanded("text-box", "cap none alphabetic").is_none());
        assert_eq!(synthesize("text-box", &["trim-both", "cap"], false).as_deref(), Some("cap"));
    }

    #[test]
    fn font_synthesis_rejects_duplicate_style_keywords() {
        assert!(expanded("font-synthesis", "style oblique-only").is_none());
        assert_eq!(expanded("font-synthesis", "position weight").unwrap(), ["auto", "none", "none", "auto"]);
        assert_eq!(synthesize("font-synthesis", &["auto", "none", "none", "auto"], false).as_deref(), Some("weight position"));
    }
}
