//! Bounded, renderer-neutral helpers for the first SVG viewport and geometry
//! subset. Path-data parsing remains in the shared image backend so Canvas and
//! inline SVG use the same validated parser.
use alloc::{format, string::String, sync::Arc, vec::Vec};

use crate::{paint::Affine, Name};

const MAX_TRANSFORM_BYTES: usize = 8 * 1024;
const MAX_TRANSFORMS: usize = 32;
const MAX_POINTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewBox {
    pub min_x: f32,
    pub min_y: f32,
    pub width: f32,
    pub height: f32,
}

/// Return an attribute by its XML local name. This keeps prefixed SVG names
/// usable in XML documents while ordinary HTML-created SVG remains unchanged.
pub fn attribute<'a>(attributes: &'a [(Name, String)], local: &str) -> Option<&'a str> {
    attributes.iter().find_map(|(name, value)| {
        crate::xml::split_qname(name.as_str())
            .map(|(_, candidate)| candidate == local)
            .unwrap_or_else(|| name.as_str() == local)
            .then_some(value.as_str())
    })
}

pub fn local_name(name: &Name) -> &str {
    crate::xml::split_qname(name.as_str())
        .map(|(_, local)| local)
        .unwrap_or_else(|| name.as_str())
}

pub fn parse_view_box(input: &str) -> Option<ViewBox> {
    let values = number_list(input, 4)?;
    if values.len() != 4 || values[2] <= 0.0 || values[3] <= 0.0 {
        return None;
    }
    Some(ViewBox {
        min_x: values[0],
        min_y: values[1],
        width: values[2],
        height: values[3],
    })
}

/// Resolve intrinsic dimensions for an inline SVG viewport. Explicit CSS
/// dimensions win over SVG attributes. One missing dimension follows a valid
/// viewBox ratio; otherwise the replaced-element default is 300 by 150 CSS px.
pub fn root_size(
    attributes: &[(Name, String)],
    css_width: Option<f32>,
    css_height: Option<f32>,
    available_width: f32,
    available_height: Option<f32>,
) -> (f32, f32) {
    let view_box = attribute(attributes, "viewBox").and_then(parse_view_box);
    let width = css_width.or_else(|| {
        attribute(attributes, "width").and_then(|value| dimension(value, available_width))
    });
    let height = css_height.or_else(|| {
        attribute(attributes, "height")
            .and_then(|value| dimension(value, available_height.unwrap_or(150.0)))
    });
    let ratio = view_box.map(|view_box| view_box.width / view_box.height);
    let width = width.filter(|width| width.is_finite() && *width >= 0.0);
    let height = height.filter(|height| height.is_finite() && *height >= 0.0);
    let (width, height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, ratio.map_or(150.0, |ratio| width / ratio)),
        (None, Some(height)) => (ratio.map_or(300.0, |ratio| height * ratio), height),
        (None, None) => (300.0, 150.0),
    };
    (width.max(0.0), height.max(0.0))
}

fn dimension(input: &str, percentage_basis: f32) -> Option<f32> {
    let input = input.trim();
    let value = if let Some(percent) = input.strip_suffix('%') {
        percent.trim().parse::<f32>().ok()? * percentage_basis / 100.0
    } else {
        input
            .strip_suffix("px")
            .unwrap_or(input)
            .trim()
            .parse::<f32>()
            .ok()?
    };
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Read a finite SVG numeric attribute, accepting the common `px` suffix for
/// lengths while leaving unit-bearing coordinates unsupported.
pub fn number_attribute(attributes: &[(Name, String)], local: &str) -> Option<f32> {
    attribute(attributes, local)
        .filter(|value| !value.trim().ends_with('%'))
        .and_then(coordinate_length)
}

/// Resolve one SVG coordinate against its viewport axis. Percentages are
/// relative to that axis; unitless numbers and CSS pixels use the user-space
/// scale directly.
pub fn coordinate_value(input: &str, percentage_basis: f32) -> Option<f32> {
    let value = input.trim();
    let value = if let Some(percent) = value.strip_suffix('%') {
        percent.trim().parse::<f32>().ok()? * percentage_basis / 100.0
    } else {
        coordinate_length(value)?
    };
    value.is_finite().then_some(value)
}

pub fn coordinate_attribute(
    attributes: &[(Name, String)],
    local: &str,
    percentage_basis: f32,
) -> Option<f32> {
    attribute(attributes, local).and_then(|value| coordinate_value(value, percentage_basis))
}

/// Whether a simple SVG length is within the numeric/px/percentage subset
/// used by the first renderer. Coordinates may be negative; dimensions are
/// validated separately by their element algorithms.
pub fn supports_length(input: &str) -> bool {
    coordinate_length(input).is_some()
        || input
            .trim()
            .strip_suffix('%')
            .and_then(|value| value.trim().parse::<f32>().ok())
            .is_some_and(f32::is_finite)
}

fn coordinate_length(input: &str) -> Option<f32> {
    let input = input.trim();
    let value = input
        .strip_suffix("px")
        .unwrap_or(input)
        .trim()
        .parse::<f32>()
        .ok()?;
    value.is_finite().then_some(value)
}

/// Map SVG user coordinates into the viewport using viewBox and the basic
/// `preserveAspectRatio` meet/slice/none forms.
pub fn view_box_transform(
    view_box: Option<ViewBox>,
    attributes: &[(Name, String)],
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) -> (Affine, ViewBox) {
    let view_box = view_box.unwrap_or(ViewBox {
        min_x: 0.0,
        min_y: 0.0,
        width: width.max(1.0),
        height: height.max(1.0),
    });
    if width <= 0.0 || height <= 0.0 {
        return (Affine::IDENTITY, view_box);
    }
    let preserve = attribute(attributes, "preserveAspectRatio").unwrap_or("xMidYMid meet");
    let mut parts = preserve.split_ascii_whitespace();
    let align = parts.next().unwrap_or("xMidYMid");
    let mode = parts.next().unwrap_or("meet");
    let sx = width / view_box.width;
    let sy = height / view_box.height;
    let (scale_x, scale_y) = if align == "none" {
        (sx, sy)
    } else {
        let scale = if mode == "slice" {
            sx.max(sy)
        } else {
            sx.min(sy)
        };
        (scale, scale)
    };
    let spare_x = width - view_box.width * scale_x;
    let spare_y = height - view_box.height * scale_y;
    let align_x = if align.contains("xMin") {
        0.0
    } else if align.contains("xMax") {
        1.0
    } else {
        0.5
    };
    let align_y = if align.contains("YMin") {
        0.0
    } else if align.contains("YMax") {
        1.0
    } else {
        0.5
    };
    (
        Affine {
            a: scale_x,
            b: 0.0,
            c: 0.0,
            d: scale_y,
            e: x + spare_x * align_x - view_box.min_x * scale_x,
            f: y + spare_y * align_y - view_box.min_y * scale_y,
        },
        view_box,
    )
}

/// Parse a bounded SVG `transform` list. This supports the affine transforms
/// used by basic SVG shapes and groups, without interpreting CSS transform
/// origin or transform-box rules.
pub fn parse_transform(input: &str) -> Option<Affine> {
    if input.len() > MAX_TRANSFORM_BYTES {
        return None;
    }
    let mut rest = input.trim();
    let mut result = Affine::IDENTITY;
    let mut count = 0;
    while !rest.is_empty() {
        if count == MAX_TRANSFORMS {
            return None;
        }
        let open = rest.find('(')?;
        let name = rest[..open].trim();
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return None;
        }
        let close = rest[open + 1..].find(')')? + open + 1;
        let args = number_list(&rest[open + 1..close], 6)?;
        let transform = match name {
            "matrix" if args.len() == 6 => Affine {
                a: args[0],
                b: args[1],
                c: args[2],
                d: args[3],
                e: args[4],
                f: args[5],
            },
            "translate" if (1..=2).contains(&args.len()) => Affine {
                e: args[0],
                f: *args.get(1).unwrap_or(&0.0),
                ..Affine::IDENTITY
            },
            "scale" if (1..=2).contains(&args.len()) => Affine {
                a: args[0],
                d: *args.get(1).unwrap_or(&args[0]),
                ..Affine::IDENTITY
            },
            "rotate" if args.len() == 1 || args.len() == 3 => {
                let angle = args[0].to_radians();
                let (sin, cos) = angle.sin_cos();
                let rotate = Affine {
                    a: cos,
                    b: sin,
                    c: -sin,
                    d: cos,
                    ..Affine::IDENTITY
                };
                if args.len() == 1 {
                    rotate
                } else {
                    Affine {
                        e: args[1],
                        f: args[2],
                        ..Affine::IDENTITY
                    }
                    .then(rotate)
                    .then(Affine {
                        e: -args[1],
                        f: -args[2],
                        ..Affine::IDENTITY
                    })
                }
            }
            "skewX" if args.len() == 1 => Affine {
                c: args[0].to_radians().tan(),
                ..Affine::IDENTITY
            },
            "skewY" if args.len() == 1 => Affine {
                b: args[0].to_radians().tan(),
                ..Affine::IDENTITY
            },
            _ => return None,
        };
        if !transform.is_finite() {
            return None;
        }
        result = result.then(transform);
        count += 1;
        rest = rest[close + 1..].trim_start();
        if rest.starts_with(',') {
            rest = rest[1..].trim_start();
        }
    }
    (count != 0).then_some(result)
}

/// Create path data for a basic SVG shape. The existing shared path-data
/// parser validates and rasterizes the result; this helper only translates
/// primitive attributes into that common representation.
pub fn shape_path(tag: &str, attributes: &[(Name, String)], user_box: ViewBox) -> Option<String> {
    shape_path_with_geometry(tag, attributes, user_box, &[const { None }; 9], None, None)
}

/// Create basic shape path data after applying the CSS geometry properties
/// that are resolved by the SVG renderer. The path-data parser remains shared
/// with Canvas and the raster backend.
pub fn shape_path_with_geometry(
    tag: &str,
    attributes: &[(Name, String)],
    user_box: ViewBox,
    geometry: &[Option<Arc<str>>; 9],
    css_width: Option<f32>,
    css_height: Option<f32>,
) -> Option<String> {
    let geometry_value = |index: usize, key: &str| {
        geometry[index]
            .as_deref()
            .or_else(|| attribute(attributes, key))
    };
    let number = |key: &str, basis: f32, default: f32| {
        attribute(attributes, key)
            .and_then(|value| dimension(value, basis))
            .unwrap_or(default)
    };
    let coordinate = |key: &str, index: usize, basis: f32| {
        geometry_value(index, key)
            .map(|value| {
                value
                    .trim()
                    .strip_suffix('%')
                    .and_then(|percent| percent.trim().parse::<f32>().ok())
                    .filter(|percent| percent.is_finite())
                    .map(|percent| percent * basis / 100.0)
                    .or_else(|| coordinate_length(value))
            })
            .unwrap_or(Some(0.0))
    };
    let attribute_coordinate = |key: &str, basis: f32| {
        attribute(attributes, key)
            .map(|value| {
                value
                    .trim()
                    .strip_suffix('%')
                    .and_then(|percent| percent.trim().parse::<f32>().ok())
                    .filter(|percent| percent.is_finite())
                    .map(|percent| percent * basis / 100.0)
                    .or_else(|| coordinate_length(value))
            })
            .unwrap_or(Some(0.0))
    };
    let path = match tag {
        "rect" => {
            let (x, y, width, height) = (
                coordinate("x", 0, user_box.width)?,
                coordinate("y", 1, user_box.height)?,
                css_width
                    .or_else(|| {
                        geometry_value(2, "width")
                            .and_then(|value| dimension(value, user_box.width))
                    })
                    .unwrap_or_else(|| number("width", user_box.width, 0.0)),
                css_height
                    .or_else(|| {
                        geometry_value(3, "height")
                            .and_then(|value| dimension(value, user_box.height))
                    })
                    .unwrap_or_else(|| number("height", user_box.height, 0.0)),
            );
            if width <= 0.0 || height <= 0.0 {
                return None;
            }
            let rx_attribute =
                geometry_value(4, "rx").and_then(|value| dimension(value, user_box.width));
            let ry_attribute =
                geometry_value(5, "ry").and_then(|value| dimension(value, user_box.height));
            let rx = rx_attribute
                .or(ry_attribute)
                .unwrap_or(0.0)
                .min(width * 0.5);
            let ry = ry_attribute
                .or(rx_attribute)
                .unwrap_or(0.0)
                .min(height * 0.5);
            if rx == 0.0 || ry == 0.0 {
                format!("M{x} {y}h{width}v{height}h-{width}Z")
            } else {
                format!(
                    "M{} {}H{}A{rx} {ry} 0 0 1 {} {}V{}A{rx} {ry} 0 0 1 {} {}H{}A{rx} {ry} 0 0 1 {} {}V{}A{rx} {ry} 0 0 1 {} {}Z",
                    x + rx,
                    y,
                    x + width - rx,
                    x + width,
                    y + ry,
                    y + height - ry,
                    x + width - rx,
                    y + height,
                    x + rx,
                    x,
                    y + height - ry,
                    y + ry,
                    x + rx,
                    y,
                )
            }
        }
        "circle" | "ellipse" => {
            let (cx, cy) = (
                coordinate("cx", 6, user_box.width)?,
                coordinate("cy", 7, user_box.height)?,
            );
            let (rx, ry) = if tag == "circle" {
                let basis = ((user_box.width * user_box.width + user_box.height * user_box.height)
                    * 0.5)
                    .sqrt();
                let radius = geometry_value(8, "r")
                    .and_then(|value| dimension(value, basis))
                    .unwrap_or_else(|| number("r", basis, 0.0));
                (radius, radius)
            } else {
                let rx = geometry_value(4, "rx").and_then(|value| dimension(value, user_box.width));
                let ry =
                    geometry_value(5, "ry").and_then(|value| dimension(value, user_box.height));
                (rx.or(ry).unwrap_or(0.0), ry.or(rx).unwrap_or(0.0))
            };
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            let left = cx - rx;
            let right = cx + rx;
            format!("M{left} {cy}A{rx} {ry} 0 1 0 {right} {cy}A{rx} {ry} 0 1 0 {left} {cy}Z")
        }
        "line" => format!(
            "M{} {}L{} {}",
            attribute_coordinate("x1", user_box.width)?,
            attribute_coordinate("y1", user_box.height)?,
            attribute_coordinate("x2", user_box.width)?,
            attribute_coordinate("y2", user_box.height)?
        ),
        "polyline" | "polygon" => {
            let points = parse_points(attribute(attributes, "points")?)?;
            if points.len() < 2 {
                return None;
            }
            let mut path = String::new();
            for (index, (x, y)) in points.into_iter().enumerate() {
                if index == 0 {
                    path.push_str(&format!("M{x} {y}"));
                } else {
                    path.push_str(&format!("L{x} {y}"));
                }
            }
            if tag == "polygon" {
                path.push('Z');
            }
            path
        }
        _ => return None,
    };
    (path.len() <= crate::paint::MAX_SVG_PATH_BYTES).then_some(path)
}

fn parse_points(input: &str) -> Option<Vec<(f32, f32)>> {
    let values = number_list(input, MAX_POINTS * 2)?;
    if values.len() < 4 || values.len() % 2 != 0 {
        return None;
    }
    Some(
        values
            .chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect(),
    )
}

fn number_list(input: &str, limit: usize) -> Option<Vec<f32>> {
    let bytes = input.as_bytes();
    let mut values = Vec::new();
    let mut offset = 0;
    let mut comma_pending = false;
    while offset < bytes.len() {
        while bytes.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if bytes.get(offset) == Some(&b',') {
            if values.is_empty() || comma_pending {
                return None;
            }
            comma_pending = true;
            offset += 1;
            continue;
        }
        if offset == bytes.len() {
            break;
        }
        let start = offset;
        if matches!(bytes[offset], b'+' | b'-') {
            offset += 1;
        }
        let mut digits = 0usize;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            digits += 1;
            offset += 1;
        }
        if bytes.get(offset) == Some(&b'.') {
            offset += 1;
            while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                digits += 1;
                offset += 1;
            }
        }
        if digits == 0 {
            return None;
        }
        if bytes
            .get(offset)
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            offset += 1;
            if bytes
                .get(offset)
                .is_some_and(|byte| matches!(byte, b'+' | b'-'))
            {
                offset += 1;
            }
            let exponent = offset;
            while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                offset += 1;
            }
            if exponent == offset {
                return None;
            }
        }
        let value = input.get(start..offset)?.parse::<f32>().ok()?;
        if !value.is_finite() || values.len() == limit {
            return None;
        }
        values.push(value);
        comma_pending = false;
        if offset < bytes.len()
            && !bytes[offset].is_ascii_whitespace()
            && bytes[offset] != b','
            && !matches!(bytes[offset], b'+' | b'-')
        {
            return None;
        }
    }
    (!comma_pending).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_dimensions_and_viewbox_mapping_follow_intrinsic_ratio() {
        let attrs = alloc::vec![
            (Name::new("viewBox"), "10 20 200 100".into()),
            (Name::new("width"), "100".into()),
        ];
        assert_eq!(root_size(&attrs, None, None, 500.0, None), (100.0, 50.0));
        let (matrix, view_box) = view_box_transform(
            Some(parse_view_box("10 20 200 100").unwrap()),
            &[],
            4.0,
            8.0,
            100.0,
            100.0,
        );
        assert_eq!(matrix.apply(view_box.min_x, view_box.min_y), (4.0, 33.0));
        assert_eq!(matrix.apply(210.0, 120.0), (104.0, 83.0));
    }

    #[test]
    fn primitive_paths_and_group_transforms_are_bounded_and_finite() {
        let attrs = alloc::vec![
            (Name::new("x"), "2".into()),
            (Name::new("y"), "3".into()),
            (Name::new("width"), "5".into()),
            (Name::new("height"), "7".into()),
        ];
        assert_eq!(
            shape_path(
                "rect",
                &attrs,
                ViewBox {
                    min_x: 0.0,
                    min_y: 0.0,
                    width: 100.0,
                    height: 100.0,
                }
            )
            .as_deref(),
            Some("M2 3h5v7h-5Z")
        );
        let matrix = parse_transform("translate(4, 5) scale(2)").unwrap();
        let point = matrix.apply(1.0, 1.0);
        assert!(point.0.is_finite() && point.1.is_finite());
        assert_eq!(parse_transform("scale(NaN)"), None);
    }

    #[test]
    fn rounded_rect_copies_a_single_corner_radius_to_both_axes() {
        let attrs = alloc::vec![
            (Name::new("width"), "20".into()),
            (Name::new("height"), "10".into()),
            (Name::new("rx"), "4".into()),
        ];
        let path = shape_path(
            "rect",
            &attrs,
            ViewBox {
                min_x: 0.0,
                min_y: 0.0,
                width: 20.0,
                height: 10.0,
            },
        )
        .unwrap();
        assert!(path.contains("A4 4 0"));
    }

    #[test]
    fn ellipse_auto_radius_uses_other_resolved_axis_without_clamping() {
        let user_box = ViewBox {
            min_x: 0.0,
            min_y: 0.0,
            width: 60.0,
            height: 40.0,
        };
        let attrs = alloc::vec![
            (Name::new("cx"), "20".into()),
            (Name::new("cy"), "20".into())
        ];
        let mut geometry: [Option<Arc<str>>; 9] = [const { None }; 9];
        geometry[4] = Some(Arc::from("25%"));
        geometry[5] = Some(Arc::from("auto"));
        let path =
            shape_path_with_geometry("ellipse", &attrs, user_box, &geometry, None, None).unwrap();
        assert!(path.contains("A15 15 0"));
        geometry[4] = Some(Arc::from("auto"));
        geometry[5] = Some(Arc::from("25%"));
        let path =
            shape_path_with_geometry("ellipse", &attrs, user_box, &geometry, None, None).unwrap();
        assert!(path.contains("A10 10 0"));
        geometry[5] = Some(Arc::from("100px"));
        let path =
            shape_path_with_geometry("ellipse", &attrs, user_box, &geometry, None, None).unwrap();
        assert!(path.contains("A100 100 0"));
        geometry[5] = Some(Arc::from("auto"));
        assert!(
            shape_path_with_geometry("ellipse", &attrs, user_box, &geometry, None, None).is_none()
        );
        geometry[4] = Some(Arc::from("0px"));
        geometry[5] = Some(Arc::from("10px"));
        assert!(
            shape_path_with_geometry("ellipse", &attrs, user_box, &geometry, None, None).is_none()
        );
    }
}
