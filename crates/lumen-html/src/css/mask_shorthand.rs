//! Serialization of the `mask` shorthand from its eight longhands, one layer per
//! comma-separated entry with initial components omitted.

pub(super) const LONGHANDS: [&str; 8] =
    ["mask-image", "mask-position", "mask-size", "mask-repeat", "mask-origin", "mask-clip", "mask-mode", "mask-composite"];

fn items(list: &str) -> alloc::vec::Vec<&str> {
    let mut parts = alloc::vec::Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for (index, ch) in list.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(list[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(list[start..].trim());
    parts
}

fn box_pair(origin: &str, clip: &str) -> Option<alloc::string::String> {
    Some(match (origin, clip) {
        ("border-box", "border-box") => return None,
        ("border-box", "no-clip") => "no-clip".into(),
        (origin, clip) if origin == clip => origin.into(),
        (origin, clip) => alloc::format!("{origin} {clip}"),
    })
}

pub(super) fn synthesize(values: &[&str]) -> Option<alloc::string::String> {
    let lists: alloc::vec::Vec<alloc::vec::Vec<&str>> = values.iter().map(|value| items(value)).collect();
    if lists.len() != LONGHANDS.len() || lists.iter().any(|list| list.is_empty()) {
        return None;
    }
    let count = lists.iter().map(|list| list.len()).max()?;
    if lists.iter().any(|list| count % list.len() != 0) {
        return None;
    }
    let mut layers = alloc::vec::Vec::with_capacity(count);
    for index in 0..count {
        let at = |slot: usize| lists[slot][index % lists[slot].len()];
        let mut parts: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
        if at(0) != "none" {
            parts.push(at(0).into());
        }
        let (position, size) = (at(1), at(2));
        if position != "0% 0%" || size != "auto" {
            parts.push(if size == "auto" { position.into() } else { alloc::format!("{position} / {size}") });
        }
        if at(3) != "repeat" {
            parts.push(at(3).into());
        }
        if let Some(boxes) = box_pair(at(4), at(5)) {
            parts.push(boxes);
        }
        if at(7) != "add" {
            parts.push(at(7).into());
        }
        if at(6) != "match-source" {
            parts.push(at(6).into());
        }
        layers.push(if parts.is_empty() { alloc::string::String::from("none") } else { parts.join(" ") });
    }
    Some(layers.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omits_initial_components_and_merges_boxes() {
        let initial = ["none", "0% 0%", "auto", "repeat", "border-box", "border-box", "match-source", "add"];
        assert_eq!(synthesize(&initial).as_deref(), Some("none"));
        let mut values = initial;
        values[0] = "url(\"a\")";
        values[1] = "1px 2px";
        values[2] = "contain";
        values[5] = "no-clip";
        values[7] = "intersect";
        assert_eq!(synthesize(&values).as_deref(), Some("url(\"a\") 1px 2px / contain no-clip intersect"));
    }
}
