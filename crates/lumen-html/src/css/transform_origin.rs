//! The three-value `transform-origin` grammar: the two-dimensional origin
//! shares the background-position parser and the optional third value is a
//! z offset length.
use super::{components, computed_values, decoded_css_keyword, static_length_context, transform_length_with_query, transform_origin_with_query,
    ContainerUnitContext, LengthContext, TransformLength};
use alloc::{string::String, vec::Vec};

pub(super) fn parse(raw: &str, context: LengthContext, query: ContainerUnitContext) -> Option<([TransformLength; 2], f32)> {
    let args = components(raw)?;
    if args.len() != 3 {
        return Some((transform_origin_with_query(raw, context, query)?, 0.0));
    }
    let z = transform_length_with_query(args[2], context, query).filter(|z| z.percent == 0.0)?;
    let plane = transform_origin_with_query(&alloc::format!("{} {}", args[0], args[1]), context, query)?;
    Some((plane, z.pixels))
}

fn horizontal(token: &str) -> bool {
    decoded_css_keyword(token, "left") || decoded_css_keyword(token, "right")
}

fn vertical(token: &str) -> bool {
    decoded_css_keyword(token, "top") || decoded_css_keyword(token, "bottom")
}

/// Canonical specified serialization: the horizontal component first and the
/// vertical second, a missing component as `center`, and a zero z omitted.
pub(super) fn specified(raw: &str) -> Option<String> {
    let args: Vec<&str> = components(raw)?;
    let (x, y) = match args.len() {
        1 => if vertical(args[0]) { ("center", args[0]) } else { (args[0], "center") },
        2 | 3 => if vertical(args[0]) || horizontal(args[1]) { (args[1], args[0]) } else { (args[0], args[1]) },
        _ => return None,
    };
    let mut output = alloc::format!("{x} {y}");
    if let Some(z) = args.get(2) {
        output.push(' ');
        output.push_str(z);
    }
    Some(output)
}

/// The computed form of a specified origin, for animation endpoints that arrive unresolved.
pub(crate) fn endpoint(raw: &str) -> Option<String> {
    if !super::length_independent(raw) {
        return None;
    }
    let context = static_length_context();
    let (plane, z) = parse(raw, context, ContainerUnitContext::no_container(context.viewport))?;
    Some(alloc::format!(
        "{} {} {}",
        computed_values::percentage(plane[0].pixels, plane[0].percent),
        computed_values::percentage(plane[1].pixels, plane[1].percent),
        computed_values::px(z)
    ))
}
