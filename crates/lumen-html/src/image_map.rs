//! Image map hit testing for `img[usemap]`: map resolution, area shape and
//! coords parsing, and point containment.
use alloc::vec::Vec;
use crate::{forms::html_element_local_name, selector::next_descendant, Document, NodeId};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Default,
    Rect,
    Circle,
    Poly,
}

fn parse_shape(value: Option<&str>) -> Shape {
    let Some(value) = value else { return Shape::Rect };
    if value.eq_ignore_ascii_case("default") {
        Shape::Default
    } else if value.eq_ignore_ascii_case("circle") || value.eq_ignore_ascii_case("circ") {
        Shape::Circle
    } else if value.eq_ignore_ascii_case("poly") || value.eq_ignore_ascii_case("polygon") {
        Shape::Poly
    } else {
        Shape::Rect
    }
}

fn is_delimiter(c: char) -> bool {
    matches!(c, ',' | ';' | ' ' | '\t' | '\n' | '\u{c}' | '\r')
}

fn leading_number(token: &str) -> f64 {
    let start = token.find(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'));
    let Some(start) = start else { return 0.0 };
    let rest = &token[start..];
    let bytes = rest.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'-' | b'+')) {
        end += 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        let fraction = end + 1;
        let mut after = fraction;
        while after < bytes.len() && bytes[after].is_ascii_digit() {
            after += 1;
        }
        if after > fraction || end > digits_start {
            end = after;
        }
    }
    if end == digits_start {
        return 0.0;
    }
    if end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
        let mut exponent = end + 1;
        if exponent < bytes.len() && matches!(bytes[exponent], b'-' | b'+') {
            exponent += 1;
        }
        let exponent_digits = exponent;
        while exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
            exponent += 1;
        }
        if exponent > exponent_digits {
            end = exponent;
        }
    }
    rest[..end].parse().unwrap_or(0.0)
}

fn parse_coords(value: Option<&str>) -> Vec<f64> {
    value
        .unwrap_or("")
        .split(is_delimiter)
        .filter(|token| !token.is_empty())
        .map(leading_number)
        .collect()
}

fn contains(shape: Shape, coords: &[f64], x: f64, y: f64) -> bool {
    match shape {
        Shape::Default => true,
        Shape::Rect => {
            if coords.len() < 4 {
                return false;
            }
            let (left, right) = (coords[0].min(coords[2]), coords[0].max(coords[2]));
            let (top, bottom) = (coords[1].min(coords[3]), coords[1].max(coords[3]));
            x >= left && x < right && y >= top && y < bottom
        }
        Shape::Circle => {
            if coords.len() < 3 || coords[2] <= 0.0 {
                return false;
            }
            let (dx, dy) = (x - coords[0], y - coords[1]);
            dx * dx + dy * dy <= coords[2] * coords[2]
        }
        Shape::Poly => {
            let count = coords.len() / 2 * 2;
            if count < 6 {
                return false;
            }
            let points = &coords[..count];
            let vertices = count / 2;
            let mut inside = false;
            let mut previous = vertices - 1;
            for current in 0..vertices {
                let (cx, cy) = (points[current * 2], points[current * 2 + 1]);
                let (px, py) = (points[previous * 2], points[previous * 2 + 1]);
                if (cy > y) != (py > y) && x < (px - cx) * (y - cy) / (py - cy) + cx {
                    inside = !inside;
                }
                previous = current;
            }
            inside
        }
    }
}

fn find_map(document: &Document, image: NodeId) -> Option<NodeId> {
    let usemap = document.get_attribute_ns_ref(image, None, "usemap").ok()??;
    let name = &usemap[usemap.find('#')? + 1..];
    if name.is_empty() {
        return None;
    }
    let root = document.root_node(image, false).ok()?;
    let mut current = Some(root);
    while let Some(node) = current {
        if html_element_local_name(document, node) == Some("map") {
            let matches = |attribute| {
                document.get_attribute_ns_ref(node, None, attribute).ok().flatten() == Some(name)
            };
            if matches("name") || matches("id") {
                return Some(node);
            }
        }
        current = next_descendant(document, root, node).ok().flatten();
    }
    None
}

/// First `area` of the image's map containing the point, given relative to the
/// image's content box origin.
pub fn area_at(document: &Document, image: NodeId, x: f64, y: f64) -> Option<NodeId> {
    if html_element_local_name(document, image) != Some("img") {
        return None;
    }
    let map = find_map(document, image)?;
    let mut current = next_descendant(document, map, map).ok().flatten();
    while let Some(node) = current {
        if html_element_local_name(document, node) == Some("area") {
            let shape = parse_shape(document.get_attribute_ns_ref(node, None, "shape").ok().flatten());
            let coords = parse_coords(document.get_attribute_ns_ref(node, None, "coords").ok().flatten());
            if contains(shape, &coords, x, y) {
                return Some(node);
            }
        }
        current = next_descendant(document, map, node).ok().flatten();
    }
    None
}
