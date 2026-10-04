//! Block formatting into a typed display list in CSS pixels.
use crate::{
    Document, Namespace, NodeId, NodeKind,
    css::{
        self, AlignItems, BorderStyle, BoxSizing, Clear, Direction, Display, FlexDirection, Float,
        JustifyContent, LineHeight, Position, Style, StyleIndex, TextAlign, WhiteSpace,
    },
    paint::{
        Affine, BackgroundBox, BackgroundImage, BackgroundLayer, BackgroundPaint, BackgroundRepeat,
        BackgroundSize, BackgroundSizeKind, Command, DisplayList, FontSpec, ImageData,
        LengthPercentage, MAX_BACKGROUND_LAYERS, Rect, Rgba, TextShaper,
    },
};
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

pub trait ImageResolver {
    fn resolve(&self, source: &str) -> ImageState;
    /// Resolve a URL reference in its declaring resource's context. Hosts
    /// which already use absolute sources can retain the ordinary resolver.
    fn resolve_from(&self, _base: &str, source: &str) -> ImageState {
        self.resolve(source)
    }
    /// Whether a decoded resource may be read back through Canvas APIs.
    /// Hosts that apply the browser's same-origin/CORS rules override this.
    fn origin_clean_from(&self, _base: &str, _source: &str) -> bool {
        true
    }
    /// Per-element resolution lets hosts apply request modes such as CORS to
    /// two elements that point at the same URL independently.
    fn resolve_node_from(&self, _node: NodeId, _base: &str, _source: &str) -> Option<ImageState> {
        None
    }
    fn node_origin_clean(&self, node: NodeId, base: &str, source: &str) -> bool {
        let _ = node;
        self.origin_clean_from(base, source)
    }
    /// Live element bitmaps, including canvases, have no URL source.
    fn resolve_node(&self, _node: NodeId) -> Option<ImageState> {
        None
    }
    fn generation(&self) -> u64 {
        0
    }
}

/// HTML's non-negative integer parser for reflected canvas bitmap dimensions.
pub fn canvas_dimension(input: Option<&str>, fallback: u32) -> u32 {
    let Some(input) = input else {
        return fallback;
    };
    let input = input.trim_start_matches(['\t', '\n', '\u{c}', '\r', ' ']);
    let negative = input.starts_with('-');
    let input = input.strip_prefix(['+', '-']).unwrap_or(input);
    let count = input.bytes().take_while(u8::is_ascii_digit).count();
    if count == 0 {
        return fallback;
    }
    match input[..count].parse::<u32>() {
        Ok(value) if !negative || value == 0 => value,
        _ => fallback,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageState {
    Ready(Arc<ImageData>),
    Pending,
    Failed,
}

impl<F: Fn(&str) -> ImageState> ImageResolver for F {
    fn resolve(&self, source: &str) -> ImageState {
        self(source)
    }
}

fn resolve_element_image(
    images: &dyn ImageResolver,
    node: NodeId,
    source: Option<&str>,
    base: Option<&str>,
) -> Option<ImageState> {
    images.resolve_node(node).or_else(|| {
        source.map(|source| {
            if let Some(base) = base {
                images.resolve_from(base, source)
            } else {
                images.resolve(source)
            }
        })
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    Css(css::CssError),
    InvalidTree,
    DepthLimit,
    Text,
    /// Generated content requires formatting behavior that this layout path
    /// cannot represent without changing the document tree.
    UnsupportedGeneratedContent,
    CommandLimit,
    ImagePending,
    ImageFailed,
    GridLimit,
}

/// SVG features which affect pixels but are not implemented by the shared
/// vector renderer. Hosts can use this result to keep conformance outcomes
/// unsupported when a document requires semantics outside the renderer.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SvgUnsupportedFeature {
    Element(String),
    Attribute(String),
    PaintServer,
    NestedViewport,
    Transform,
    Length(String),
    TextLayout,
}

const SVG_UNSUPPORTED_ATTRIBUTES: &[&str] = &[
    "alignment-baseline",
    "baseline-shift",
    "dominant-baseline",
    "dx",
    "dy",
    "fill-opacity",
    "filter",
    "lengthAdjust",
    "marker-end",
    "marker-mid",
    "marker-start",
    "mask",
    "paint-order",
    "pathLength",
    "rotate",
    "shape-rendering",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "text-anchor",
    "textLength",
    "text-rendering",
    "vector-effect",
];

const SVG_INERT_DEFINITION_ELEMENTS: &[&str] = &[
    "clipPath",
    "defs",
    "desc",
    "filter",
    "linearGradient",
    "marker",
    "mask",
    "metadata",
    "pattern",
    "radialGradient",
    "script",
    "stop",
    "style",
    "symbol",
    "title",
];

const SVG_SUPPORTED_ELEMENTS: &[&str] = &[
    "a", "circle", "ellipse", "g", "line", "path", "polygon", "polyline", "rect", "svg", "text",
    "tspan", "use",
];

/// Inspect the actual document tree and current computed SVG styles. This is
/// shared by render hosts so an SVG with a paint server, unsupported element,
/// or unimplemented rendering attribute is not reported as a genuine pass.
pub fn unsupported_svg_features(
    document: &Document,
    rules: &StyleIndex,
) -> Result<Vec<SvgUnsupportedFeature>, LayoutError> {
    let mut features = BTreeSet::new();
    let mut styles = css::StyleCache::default();
    let mut use_chain = Vec::new();
    let mut pending = Vec::new();
    let mut child = document
        .first_child(document.root())
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(node) = child {
        child = document
            .next_sibling(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        pending.push((node, None, 0usize, false));
    }
    while let Some((node, parent_style, parent_svg_depth, in_text)) = pending.pop() {
        let kind = document.kind(node).map_err(|_| LayoutError::InvalidTree)?;
        let NodeKind::Element {
            name,
            namespace,
            attributes,
        } = kind
        else {
            continue;
        };
        let style =
            css::compute_node_cached(document, node, parent_style.as_ref(), rules, &mut styles)
                .map_err(LayoutError::Css)?;
        if style.display == Display::None {
            continue;
        }
        let tag = crate::svg::local_name(name);
        let is_svg = *namespace == Namespace::Svg;
        let svg_depth = parent_svg_depth + usize::from(is_svg && tag == "svg");
        let in_svg = is_svg || parent_svg_depth > 0;
        if in_svg && is_svg {
            for (index, value) in style.svg_geometry.iter().enumerate() {
                if value.is_some() && !svg_geometry_supported_for_tag(tag, index) {
                    features.insert(SvgUnsupportedFeature::Attribute(
                        ["x", "y", "width", "height", "rx", "ry", "cx", "cy", "r"][index].into(),
                    ));
                }
            }
            if tag == "svg" && parent_svg_depth > 0 {
                features.insert(SvgUnsupportedFeature::NestedViewport);
            } else if !SVG_SUPPORTED_ELEMENTS.contains(&tag)
                && !SVG_INERT_DEFINITION_ELEMENTS.contains(&tag)
                && tag != "foreignObject"
            {
                features.insert(SvgUnsupportedFeature::Element(tag.into()));
            }
            if tag == "foreignObject" {
                features.insert(SvgUnsupportedFeature::Element(tag.into()));
            }
            if matches!(&style.svg_fill, css::SvgPaint::Unsupported(_))
                || matches!(&style.svg_stroke, css::SvgPaint::Unsupported(_))
            {
                features.insert(SvgUnsupportedFeature::PaintServer);
            }
            for paint in [&style.svg_fill, &style.svg_stroke] {
                if let css::SvgPaint::Reference(reference, fallback) = paint {
                    if !svg_gradient_reference_supported(
                        document,
                        rules,
                        reference,
                        &mut Vec::new(),
                        0,
                    )? && fallback.is_none()
                    {
                        features.insert(SvgUnsupportedFeature::PaintServer);
                    }
                }
            }
            if let Some(reference) = style.svg_clip_path.as_deref() {
                if tag == "svg" {
                    features.insert(SvgUnsupportedFeature::Attribute("clip-path".into()));
                } else if !svg_clip_reference_supported(document, rules, reference, tag)? {
                    features.insert(SvgUnsupportedFeature::PaintServer);
                }
            }
            for property in rules.unsupported_svg_properties_for_node(document, node) {
                if !matches!(property.as_str(), "width" | "height")
                    || !matches!(tag, "rect" | "svg")
                {
                    features.insert(SvgUnsupportedFeature::Attribute(property));
                }
            }
            for (attribute, value) in attributes {
                let local = crate::svg::local_name(attribute);
                if SVG_UNSUPPORTED_ATTRIBUTES.contains(&local) {
                    features.insert(SvgUnsupportedFeature::Attribute(local.into()));
                }
                if local == "style" {
                    for property in css::unsupported_svg_style_properties(value) {
                        if !matches!(property.as_str(), "width" | "height")
                            || !matches!(tag, "rect" | "svg")
                        {
                            features.insert(SvgUnsupportedFeature::Attribute(property));
                        }
                    }
                }
                if local == "transform" && crate::svg::parse_transform(value).is_none() {
                    features.insert(SvgUnsupportedFeature::Transform);
                }
                if local == "preserveAspectRatio" && !valid_preserve_aspect_ratio(value) {
                    features.insert(SvgUnsupportedFeature::Attribute(local.into()));
                }
                let text_percentage =
                    matches!(tag, "text" | "tspan") && value.trim().ends_with('%');
                if svg_length_attribute(tag, local)
                    && (text_percentage || !crate::svg::supports_length(value))
                {
                    features.insert(SvgUnsupportedFeature::Length(local.into()));
                }
            }
            if tag == "path"
                && crate::svg::attribute(attributes, "d")
                    .is_some_and(|data| data.len() > crate::paint::MAX_SVG_PATH_BYTES)
            {
                features.insert(SvgUnsupportedFeature::Length("d".into()));
            }
            if tag == "use" {
                if !svg_use_reference_supported(
                    document,
                    rules,
                    node,
                    &style,
                    &mut styles,
                    &mut use_chain,
                    0,
                )? {
                    features.insert(SvgUnsupportedFeature::Element("use-reference".into()));
                }
                if crate::svg::attribute(attributes, "width").is_some()
                    || crate::svg::attribute(attributes, "height").is_some()
                    || style.width.is_some()
                    || style.height.is_some()
                {
                    features.insert(SvgUnsupportedFeature::NestedViewport);
                }
            }
            if in_text && tag != "tspan" {
                features.insert(SvgUnsupportedFeature::TextLayout);
            }
            if tag == "tspan"
                && parent_style.as_ref().is_some_and(|parent| {
                    style.font_size != parent.font_size
                        || style.font_spec() != parent.font_spec()
                        || style.color != parent.color
                        || style.svg_fill != parent.svg_fill
                        || style.opacity != parent.opacity
                })
            {
                features.insert(SvgUnsupportedFeature::TextLayout);
            }
        }
        if in_svg && SVG_INERT_DEFINITION_ELEMENTS.contains(&tag) {
            continue;
        }
        let child_in_text = in_text || (is_svg && tag == "text");
        let mut child = document
            .first_child(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = child {
            child = document
                .next_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
            pending.push((current, Some(style.clone()), svg_depth, child_in_text));
        }
    }
    Ok(features.into_iter().collect())
}

fn svg_element_with_id(document: &Document, id: &str) -> Result<Option<NodeId>, LayoutError> {
    if id.is_empty() || id.len() > 1024 {
        return Ok(None);
    }
    let mut pending = Vec::new();
    let mut child = document
        .first_child(document.root())
        .map_err(|_| LayoutError::InvalidTree)?;
    let mut roots = Vec::new();
    while let Some(node) = child {
        roots.push(node);
        child = document
            .next_sibling(node)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    pending.extend(roots.into_iter().rev());
    let mut visited = 0usize;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > 65_536 {
            return Ok(None);
        }
        if let NodeKind::Element {
            namespace: Namespace::Svg,
            attributes,
            ..
        } = document.kind(node).map_err(|_| LayoutError::InvalidTree)?
        {
            if crate::svg::attribute(attributes, "id") == Some(id) {
                return Ok(Some(node));
            }
        }
        let mut next = document
            .first_child(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        let mut children = Vec::new();
        while let Some(current) = next {
            children.push(current);
            next = document
                .next_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(None)
}

fn svg_href_from_document(
    document: &Document,
    node: NodeId,
    attributes: &[(crate::Name, String)],
) -> Option<String> {
    crate::svg::attribute(attributes, "href")
        .map(String::from)
        .or_else(|| {
            document
                .get_attribute_ns(node, Some("http://www.w3.org/1999/xlink"), "href")
                .ok()
                .flatten()
        })
}

fn svg_gradient_reference_supported(
    document: &Document,
    rules: &StyleIndex,
    reference: &str,
    seen: &mut Vec<NodeId>,
    depth: usize,
) -> Result<bool, LayoutError> {
    if depth >= 32 {
        return Ok(false);
    }
    let Some(id) = reference.strip_prefix('#') else {
        return Ok(false);
    };
    let Some(node) = svg_element_with_id(document, id)? else {
        return Ok(false);
    };
    if seen.contains(&node) {
        return Ok(false);
    }
    let NodeKind::Element {
        namespace: Namespace::Svg,
        name,
        attributes,
    } = document.kind(node).map_err(|_| LayoutError::InvalidTree)?
    else {
        return Ok(false);
    };
    let tag = crate::svg::local_name(name);
    let linear = tag == "linearGradient";
    if !linear && tag != "radialGradient" {
        return Ok(false);
    }
    if !rules
        .unsupported_svg_properties_for_node(document, node)
        .is_empty()
        || crate::svg::attribute(attributes, "style")
            .is_some_and(|style| !css::unsupported_svg_style_properties(style).is_empty())
    {
        return Ok(false);
    }
    let units = match crate::svg::attribute(attributes, "gradientUnits") {
        Some("objectBoundingBox") => crate::paint::SvgGradientUnits::ObjectBoundingBox,
        Some("userSpaceOnUse") => crate::paint::SvgGradientUnits::UserSpaceOnUse,
        Some(_) => return Ok(false),
        None => crate::paint::SvgGradientUnits::ObjectBoundingBox,
    };
    if crate::svg::attribute(attributes, "spreadMethod").is_some_and(|method| method != "pad")
        || crate::svg::attribute(attributes, "gradientTransform").is_some_and(|value| {
            crate::svg::parse_transform(value).is_none_or(|transform| transform.inverse().is_none())
        })
    {
        return Ok(false);
    }
    let box_ = crate::svg::ViewBox {
        min_x: 0.0,
        min_y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    let coordinate_is_valid = |key: &str, axis: usize| {
        crate::svg::attribute(attributes, key)
            .is_none_or(|value| parse_svg_gradient_coordinate(value, units, axis, box_).is_some())
    };
    let coords_valid = if linear {
        coordinate_is_valid("x1", 0)
            && coordinate_is_valid("y1", 1)
            && coordinate_is_valid("x2", 0)
            && coordinate_is_valid("y2", 1)
    } else {
        coordinate_is_valid("cx", 0)
            && coordinate_is_valid("cy", 1)
            && coordinate_is_valid("fx", 0)
            && coordinate_is_valid("fy", 1)
            && crate::svg::attribute(attributes, "r").is_none_or(|value| {
                parse_svg_gradient_radius(value, units, box_).is_some_and(|radius| radius > 0.0)
            })
            && crate::svg::attribute(attributes, "fr").is_none_or(|value| {
                parse_svg_gradient_radius(value, units, box_).is_some_and(|radius| radius >= 0.0)
            })
    };
    if !coords_valid {
        return Ok(false);
    }
    if linear && svg_href_from_document(document, node, attributes).is_none() {
        let default_end_x = match units {
            crate::paint::SvgGradientUnits::ObjectBoundingBox => 1.0,
            crate::paint::SvgGradientUnits::UserSpaceOnUse => box_.min_x + box_.width,
        };
        let start = [
            crate::svg::attribute(attributes, "x1")
                .and_then(|value| parse_svg_gradient_coordinate(value, units, 0, box_))
                .unwrap_or(0.0),
            crate::svg::attribute(attributes, "y1")
                .and_then(|value| parse_svg_gradient_coordinate(value, units, 1, box_))
                .unwrap_or(0.0),
        ];
        let end = [
            crate::svg::attribute(attributes, "x2")
                .and_then(|value| parse_svg_gradient_coordinate(value, units, 0, box_))
                .unwrap_or(default_end_x),
            crate::svg::attribute(attributes, "y2")
                .and_then(|value| parse_svg_gradient_coordinate(value, units, 1, box_))
                .unwrap_or(0.0),
        ];
        if start == end {
            return Ok(false);
        }
    }

    seen.push(node);
    let mut href_parent = None;
    if let Some(href) = svg_href_from_document(document, node, attributes) {
        let Some(parent_id) = href.strip_prefix('#') else {
            seen.pop();
            return Ok(false);
        };
        let Some(parent) = svg_element_with_id(document, parent_id)? else {
            seen.pop();
            return Ok(false);
        };
        let NodeKind::Element {
            namespace: Namespace::Svg,
            name: parent_name,
            attributes: parent_attributes,
        } = document
            .kind(parent)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            seen.pop();
            return Ok(false);
        };
        let parent_tag = crate::svg::local_name(parent_name);
        if (linear && parent_tag != "linearGradient") || (!linear && parent_tag != "radialGradient")
        {
            seen.pop();
            return Ok(false);
        }
        // Coordinate overrides on an inherited gradient need the original
        // coordinate-unit provenance, which this bounded renderer does not
        // yet retain. Inherit-only references remain exact.
        let coordinate_keys: &[&str] = if linear {
            &["x1", "y1", "x2", "y2"]
        } else {
            &["cx", "cy", "fx", "fy", "r", "fr"]
        };
        if crate::svg::attribute(attributes, "gradientUnits").is_some()
            && crate::svg::attribute(attributes, "gradientUnits")
                != crate::svg::attribute(parent_attributes, "gradientUnits")
            && coordinate_keys
                .iter()
                .any(|key| crate::svg::attribute(attributes, key).is_none())
        {
            seen.pop();
            return Ok(false);
        }
        if !svg_gradient_reference_supported(document, rules, &href, seen, depth + 1)? {
            seen.pop();
            return Ok(false);
        }
        href_parent = Some(parent);
    }
    let mut stops = 0usize;
    let mut child = document
        .first_child(node)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(stop) = child {
        child = document
            .next_sibling(stop)
            .map_err(|_| LayoutError::InvalidTree)?;
        let NodeKind::Element {
            namespace: Namespace::Svg,
            name,
            attributes,
        } = document.kind(stop).map_err(|_| LayoutError::InvalidTree)?
        else {
            continue;
        };
        if crate::svg::local_name(name) != "stop" {
            continue;
        }
        stops += 1;
        if stops > crate::paint::MAX_SVG_GRADIENT_STOPS
            || crate::svg::attribute(attributes, "offset")
                .is_some_and(|value| parse_svg_stop_offset(value).is_none())
        {
            seen.pop();
            return Ok(false);
        }
    }
    let inherited_has_stops = if let Some(parent) = href_parent {
        let mut child = document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        let mut found = false;
        while let Some(stop) = child {
            child = document
                .next_sibling(stop)
                .map_err(|_| LayoutError::InvalidTree)?;
            if matches!(document.kind(stop), Ok(NodeKind::Element { namespace: Namespace::Svg, ref name, .. }) if crate::svg::local_name(name) == "stop")
            {
                found = true;
                break;
            }
        }
        found
    } else {
        false
    };
    let supported = stops > 0 || inherited_has_stops;
    seen.pop();
    Ok(supported)
}

fn svg_clip_reference_supported(
    document: &Document,
    rules: &StyleIndex,
    reference: &str,
    target_tag: &str,
) -> Result<bool, LayoutError> {
    let Some(id) = reference.strip_prefix('#') else {
        return Ok(false);
    };
    let Some(node) = svg_element_with_id(document, id)? else {
        return Ok(false);
    };
    let NodeKind::Element {
        namespace: Namespace::Svg,
        name,
        attributes,
    } = document.kind(node).map_err(|_| LayoutError::InvalidTree)?
    else {
        return Ok(false);
    };
    let units = crate::svg::attribute(attributes, "clipPathUnits").unwrap_or("userSpaceOnUse");
    if crate::svg::local_name(name) != "clipPath"
        || !matches!(units, "userSpaceOnUse" | "objectBoundingBox")
        || (units == "objectBoundingBox"
            && !matches!(
                target_tag,
                "path" | "rect" | "circle" | "ellipse" | "line" | "polygon" | "polyline"
            ))
        || crate::svg::attribute(attributes, "transform")
            .is_some_and(|value| crate::svg::parse_transform(value).is_none())
    {
        return Ok(false);
    }
    if !rules
        .unsupported_svg_properties_for_node(document, node)
        .is_empty()
        || crate::svg::attribute(attributes, "style")
            .is_some_and(|style| !css::unsupported_svg_style_properties(style).is_empty())
    {
        return Ok(false);
    }
    let mut pending = Vec::new();
    let mut child = document
        .first_child(node)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        pending.push((current, 0usize));
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    let mut count = 0usize;
    let mut cache = css::StyleCache::default();
    while let Some((current, depth)) = pending.pop() {
        if depth > 32 {
            return Ok(false);
        }
        let kind = document
            .kind(current)
            .map_err(|_| LayoutError::InvalidTree)?;
        let (name, attributes) = match kind {
            NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                attributes,
            } => (name, attributes),
            NodeKind::Element { .. } => return Ok(false),
            NodeKind::Text(text) if !text.trim().is_empty() => return Ok(false),
            _ => continue,
        };
        let tag = crate::svg::local_name(name);
        let style = css::compute_node_cached(document, current, None, rules, &mut cache)
            .map_err(LayoutError::Css)?;
        if style.svg_clip_path.is_some() {
            return Ok(false);
        }
        if style
            .svg_geometry
            .iter()
            .enumerate()
            .any(|(index, value)| value.is_some() && !svg_geometry_supported_for_tag(tag, index))
        {
            return Ok(false);
        }
        if matches!(tag, "title" | "desc" | "metadata" | "defs") {
            continue;
        }
        if crate::svg::attribute(attributes, "transform")
            .is_some_and(|value| crate::svg::parse_transform(value).is_none())
        {
            return Ok(false);
        }
        if rules
            .unsupported_svg_properties_for_node(document, current)
            .into_iter()
            .any(|property| !matches!(property.as_str(), "width" | "height") || tag != "rect")
        {
            return Ok(false);
        }
        if let Some(inline) = crate::svg::attribute(attributes, "style") {
            if css::unsupported_svg_style_properties(inline)
                .into_iter()
                .any(|property| !matches!(property.as_str(), "width" | "height") || tag != "rect")
            {
                return Ok(false);
            }
        }
        if matches!(tag, "g" | "a") {
            let mut nested = document
                .first_child(current)
                .map_err(|_| LayoutError::InvalidTree)?;
            while let Some(grandchild) = nested {
                pending.push((grandchild, depth + 1));
                nested = document
                    .next_sibling(grandchild)
                    .map_err(|_| LayoutError::InvalidTree)?;
            }
            continue;
        }
        if !matches!(
            tag,
            "path" | "rect" | "circle" | "ellipse" | "line" | "polygon" | "polyline"
        ) {
            return Ok(false);
        }
        for (attribute, value) in attributes {
            let local = crate::svg::local_name(attribute);
            if SVG_UNSUPPORTED_ATTRIBUTES.contains(&local)
                || (svg_length_attribute(tag, local) && !crate::svg::supports_length(value))
            {
                return Ok(false);
            }
        }
        if tag == "path"
            && crate::svg::attribute(attributes, "d")
                .is_some_and(|data| data.len() > crate::paint::MAX_SVG_PATH_BYTES)
        {
            return Ok(false);
        }
        if style.display != Display::None {
            count += 1;
        }
        if count > crate::paint::MAX_SVG_CLIP_PATHS {
            return Ok(false);
        }
    }
    Ok(count > 0)
}

fn svg_use_reference_supported(
    document: &Document,
    rules: &StyleIndex,
    use_node: NodeId,
    use_style: &Style,
    styles: &mut css::StyleCache,
    chain: &mut Vec<NodeId>,
    depth: usize,
) -> Result<bool, LayoutError> {
    if depth >= 32 {
        return Ok(false);
    }
    let NodeKind::Element { attributes, .. } = document
        .kind(use_node)
        .map_err(|_| LayoutError::InvalidTree)?
    else {
        return Ok(false);
    };
    let Some(reference) = svg_href_from_document(document, use_node, attributes) else {
        return Ok(false);
    };
    let Some(id) = reference.strip_prefix('#') else {
        return Ok(false);
    };
    let Some(target) = svg_element_with_id(document, id)? else {
        return Ok(false);
    };
    if chain.contains(&target) {
        return Ok(false);
    }
    chain.push(target);
    let supported =
        svg_use_target_supported(document, rules, target, use_style, styles, chain, depth + 1)?;
    chain.pop();
    Ok(supported)
}

fn svg_use_target_supported(
    document: &Document,
    rules: &StyleIndex,
    target: NodeId,
    parent_style: &Style,
    styles: &mut css::StyleCache,
    chain: &mut Vec<NodeId>,
    depth: usize,
) -> Result<bool, LayoutError> {
    if depth >= 32 {
        return Ok(false);
    }
    let NodeKind::Element {
        namespace: Namespace::Svg,
        name,
        attributes,
    } = document
        .kind(target)
        .map_err(|_| LayoutError::InvalidTree)?
    else {
        return Ok(false);
    };
    let tag = crate::svg::local_name(name);
    if !matches!(
        tag,
        "a" | "g"
            | "use"
            | "rect"
            | "circle"
            | "ellipse"
            | "line"
            | "path"
            | "polygon"
            | "polyline"
    ) {
        return Ok(false);
    }
    let style = css::compute_node_cached(document, target, Some(parent_style), rules, styles)
        .map_err(LayoutError::Css)?;
    if style.display == Display::None {
        return Ok(true);
    }
    if style
        .svg_geometry
        .iter()
        .enumerate()
        .any(|(index, value)| value.is_some() && !svg_geometry_supported_for_tag(tag, index))
    {
        return Ok(false);
    }
    if matches!(&style.svg_fill, css::SvgPaint::Unsupported(_))
        || matches!(&style.svg_stroke, css::SvgPaint::Unsupported(_))
    {
        return Ok(false);
    }
    for paint in [&style.svg_fill, &style.svg_stroke] {
        if let css::SvgPaint::Reference(reference, fallback) = paint {
            if !svg_gradient_reference_supported(document, rules, reference, &mut Vec::new(), 0)?
                && fallback.is_none()
            {
                return Ok(false);
            }
        }
    }
    if let Some(reference) = style.svg_clip_path.as_deref() {
        if !svg_clip_reference_supported(document, rules, reference, tag)? {
            return Ok(false);
        }
    }
    if rules
        .unsupported_svg_properties_for_node(document, target)
        .into_iter()
        .any(|property| !matches!(property.as_str(), "width" | "height") || tag != "rect")
    {
        return Ok(false);
    }
    for (attribute, value) in attributes {
        let local = crate::svg::local_name(attribute);
        if SVG_UNSUPPORTED_ATTRIBUTES.contains(&local)
            || (local == "transform" && crate::svg::parse_transform(value).is_none())
            || (svg_length_attribute(tag, local) && !crate::svg::supports_length(value))
        {
            return Ok(false);
        }
        if local == "style"
            && css::unsupported_svg_style_properties(value)
                .into_iter()
                .any(|property| !matches!(property.as_str(), "width" | "height") || tag != "rect")
        {
            return Ok(false);
        }
    }
    if tag == "path"
        && crate::svg::attribute(attributes, "d")
            .is_some_and(|data| data.len() > crate::paint::MAX_SVG_PATH_BYTES)
    {
        return Ok(false);
    }
    if tag == "use" {
        if crate::svg::attribute(attributes, "width").is_some()
            || crate::svg::attribute(attributes, "height").is_some()
            || style.width.is_some()
            || style.height.is_some()
        {
            return Ok(false);
        }
        return svg_use_reference_supported(
            document,
            rules,
            target,
            &style,
            styles,
            chain,
            depth + 1,
        );
    }
    if matches!(tag, "g" | "a") {
        let mut child = document
            .first_child(target)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            child = document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let kind = document.kind(node).map_err(|_| LayoutError::InvalidTree)?;
            if let NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            } = &kind
            {
                if matches!(
                    crate::svg::local_name(name),
                    "title" | "desc" | "metadata" | "defs"
                ) {
                    continue;
                }
            } else if let NodeKind::Text(text) = &kind {
                if !text.trim().is_empty() {
                    return Ok(false);
                }
                continue;
            } else if !matches!(kind, NodeKind::Element { .. }) {
                continue;
            }
            if !svg_use_target_supported(document, rules, node, &style, styles, chain, depth + 1)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn svg_length_attribute(tag: &str, attribute: &str) -> bool {
    match tag {
        "svg" => matches!(attribute, "width" | "height"),
        "rect" => matches!(attribute, "x" | "y" | "width" | "height" | "rx" | "ry"),
        "circle" => matches!(attribute, "cx" | "cy" | "r"),
        "ellipse" => matches!(attribute, "cx" | "cy" | "rx" | "ry"),
        "line" => matches!(attribute, "x1" | "y1" | "x2" | "y2"),
        "text" => matches!(attribute, "x" | "y"),
        "tspan" => matches!(attribute, "x" | "y" | "dx" | "dy"),
        "use" => matches!(attribute, "x" | "y" | "width" | "height"),
        _ => false,
    }
}

fn svg_geometry_supported_for_tag(tag: &str, index: usize) -> bool {
    match index {
        0 | 1 => matches!(tag, "rect" | "use"),
        4 | 5 => matches!(tag, "rect" | "ellipse"),
        6 | 7 => matches!(tag, "circle" | "ellipse"),
        8 => tag == "circle",
        _ => false,
    }
}

fn valid_preserve_aspect_ratio(value: &str) -> bool {
    let mut parts = value.split_ascii_whitespace();
    let align = parts.next().unwrap_or("xMidYMid");
    let mode = parts.next();
    let valid_align = align == "none"
        || matches!(
            align,
            "xMinYMin"
                | "xMidYMin"
                | "xMaxYMin"
                | "xMinYMid"
                | "xMidYMid"
                | "xMaxYMid"
                | "xMinYMax"
                | "xMidYMax"
                | "xMaxYMax"
        );
    valid_align
        && mode.is_none_or(|mode| matches!(mode, "meet" | "slice"))
        && parts.next().is_none()
}

fn parse_svg_gradient_coordinate(
    input: &str,
    units: crate::paint::SvgGradientUnits,
    axis: usize,
    user_box: crate::svg::ViewBox,
) -> Option<f32> {
    let input = input.trim();
    if let Some(percent) = input.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()? / 100.0;
        if !percent.is_finite() {
            return None;
        }
        return Some(match units {
            crate::paint::SvgGradientUnits::ObjectBoundingBox => percent,
            crate::paint::SvgGradientUnits::UserSpaceOnUse if axis == 0 => {
                user_box.min_x + percent * user_box.width
            }
            crate::paint::SvgGradientUnits::UserSpaceOnUse => {
                user_box.min_y + percent * user_box.height
            }
        });
    }
    if units == crate::paint::SvgGradientUnits::ObjectBoundingBox {
        let value = input
            .parse::<f32>()
            .ok()
            .or_else(|| svg_absolute_length(input))?;
        return value.is_finite().then_some(value);
    }
    let value = input
        .parse::<f32>()
        .ok()
        .or_else(|| svg_absolute_length(input))?;
    value.is_finite().then_some(value)
}

fn parse_svg_gradient_radius(
    input: &str,
    units: crate::paint::SvgGradientUnits,
    user_box: crate::svg::ViewBox,
) -> Option<f32> {
    let input = input.trim();
    let value = if let Some(percent) = input.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()? / 100.0;
        let basis = match units {
            crate::paint::SvgGradientUnits::ObjectBoundingBox => 1.0,
            crate::paint::SvgGradientUnits::UserSpaceOnUse => {
                ((user_box.width * user_box.width + user_box.height * user_box.height) * 0.5).sqrt()
            }
        };
        percent * basis
    } else {
        input
            .parse::<f32>()
            .ok()
            .or_else(|| svg_absolute_length(input))?
    };
    value.is_finite().then_some(value)
}

fn svg_absolute_length(input: &str) -> Option<f32> {
    let input = input.trim();
    let (number, scale) = [
        ("px", 1.0),
        ("in", 96.0),
        ("cm", 96.0 / 2.54),
        ("mm", 96.0 / 25.4),
        ("q", 96.0 / 101.6),
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
    ]
    .into_iter()
    .find_map(|(suffix, scale)| input.strip_suffix(suffix).map(|number| (number, scale)))?;
    let value = number.trim().parse::<f32>().ok()? * scale;
    value.is_finite().then_some(value)
}

fn parse_svg_stop_offset(input: &str) -> Option<f32> {
    let input = input.trim();
    let value = if let Some(percent) = input.strip_suffix('%') {
        percent.trim().parse::<f32>().ok()? / 100.0
    } else {
        input.parse::<f32>().ok()?
    };
    value.is_finite().then_some(value)
}

const MAX_DISPLAY_COMMANDS: usize = 16 * 1024;
const MAX_DISPLAY_LIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETAINED_FRAGMENT_ENTRIES: usize = 2048;
const MAX_RETAINED_FRAGMENT_BYTES: usize = 1 * 1024 * 1024;
const MAX_CONTROL_TEXT_BYTES: usize = lumen_common::bidi::MAX_TEXT_BYTES;
const MAX_COLLAPSED_BORDER_CANDIDATES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetainedLayoutCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Clone, PartialEq)]
struct RetainedFragmentKey {
    node: NodeId,
    parent_style: Style,
    style: Option<Style>,
    x: f32,
    y: f32,
    available: f32,
    depth: usize,
    parent_height: Option<f32>,
    containing_block: Option<Rect>,
    fixed_containing_block: Option<Rect>,
    sticky_bounds: Option<Rect>,
    cull: Rect,
    viewport: Rect,
    scroll_hazards: usize,
    transform_depth: usize,
    next_paint_order: u64,
    suppressed_border_node: Option<NodeId>,
    body_background_on_canvas: bool,
    collapsed_bottom: Option<f32>,
    was_through: Option<f32>,
    decorations: Vec<TextDecoration>,
}

#[derive(Clone)]
struct RetainedFragment {
    key: RetainedFragmentKey,
    advance: f32,
    collapsed_bottom: Option<f32>,
    was_through: Option<f32>,
    commands: Arc<[Command]>,
    hits: Arc<[HitRegion]>,
    transforms: Arc<[HitTransform]>,
    rounded_clips: Arc<[HitClip]>,
    scroll_extents: Arc<[ScrollOffset]>,
    scroll_regions: Arc<[ScrollRegion]>,
    control_text_runs: Arc<[ControlTextRun]>,
    bytes: usize,
    last_used: u64,
}

/// Bounded command/geometry fragments retained between RenderSession frames.
#[derive(Default)]
pub(crate) struct RetainedLayoutCache {
    entries: Vec<RetainedFragment>,
    bytes: usize,
    clock: u64,
    hits: usize,
    misses: usize,
}

impl RetainedLayoutCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.hits = 0;
        self.misses = 0;
    }

    pub(crate) fn begin_frame(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.clock = self.clock.saturating_add(1);
    }

    pub(crate) fn stats(&self) -> RetainedLayoutCacheStats {
        RetainedLayoutCacheStats {
            hits: self.hits,
            misses: self.misses,
            entries: self.entries.len(),
            bytes: self.bytes,
        }
    }

    pub(crate) fn invalidate_text_targets(&mut self, document: &Document, targets: &[NodeId]) {
        self.entries.retain(|entry| {
            !targets
                .iter()
                .any(|target| document_is_ancestor(document, entry.key.node, *target))
        });
        self.recount_bytes();
    }

    pub(crate) fn invalidate_subtree_targets(&mut self, document: &Document, targets: &[NodeId]) {
        self.entries.retain(|entry| {
            document.kind(entry.key.node).is_ok()
                && !targets.iter().any(|target| {
                    document_is_ancestor(document, entry.key.node, *target)
                        || document_is_ancestor(document, *target, entry.key.node)
                })
        });
        self.recount_bytes();
    }

    fn lookup(&mut self, key: &RetainedFragmentKey) -> Option<RetainedFragment> {
        self.clock = self.clock.saturating_add(1);
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key.node == key.node);
        if let Some(index) = index {
            if self.entries[index].key == *key {
                self.entries[index].last_used = self.clock;
                self.hits = self.hits.saturating_add(1);
                return Some(self.entries[index].clone());
            }
            self.misses = self.misses.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        None
    }

    fn store(&mut self, mut fragment: RetainedFragment) {
        if fragment.bytes == 0 || fragment.bytes > MAX_RETAINED_FRAGMENT_BYTES {
            return;
        }
        self.clock = self.clock.saturating_add(1);
        fragment.last_used = self.clock;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.key.node == fragment.key.node)
        {
            self.bytes = self.bytes.saturating_sub(self.entries[index].bytes);
            self.entries.remove(index);
        }
        while self.entries.len() >= MAX_RETAINED_FRAGMENT_ENTRIES
            || self.bytes.saturating_add(fragment.bytes) > MAX_RETAINED_FRAGMENT_BYTES
        {
            let Some((index, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                return;
            };
            self.bytes = self.bytes.saturating_sub(self.entries[index].bytes);
            self.entries.remove(index);
        }
        if self.entries.try_reserve(1).is_err() {
            return;
        }
        self.bytes += fragment.bytes;
        self.entries.push(fragment);
    }

    fn recount_bytes(&mut self) {
        self.bytes = self.entries.iter().map(|entry| entry.bytes).sum();
    }
}

fn document_is_ancestor(document: &Document, ancestor: NodeId, node: NodeId) -> bool {
    let mut current = Some(node);
    for _ in 0..=512 {
        let Some(id) = current else {
            return false;
        };
        if id == ancestor {
            return true;
        }
        current = match document.composed_parent(id) {
            Ok(parent) => parent,
            Err(_) => return true,
        };
    }
    true
}

fn clone_arc_slice<T: Clone>(values: &[T]) -> Option<Arc<[T]>> {
    Some(Arc::from(clone_vec(values)?.into_boxed_slice()))
}

fn clone_vec<T: Clone>(values: &[T]) -> Option<Vec<T>> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(values.len()).ok()?;
    copy.extend_from_slice(values);
    Some(copy)
}

fn relative_range(range: Range<usize>, base: usize) -> Option<Range<usize>> {
    let start = range.start.checked_sub(base)?;
    let end = range.end.checked_sub(base)?;
    (start <= end).then_some(start..end)
}

fn rebase_range(range: Range<usize>, base: usize) -> Result<Range<usize>, LayoutError> {
    let start = base
        .checked_add(range.start)
        .ok_or(LayoutError::InvalidTree)?;
    let end = base
        .checked_add(range.end)
        .ok_or(LayoutError::InvalidTree)?;
    if start > end {
        return Err(LayoutError::InvalidTree);
    }
    Ok(start..end)
}

fn retained_fragment_bytes(
    key: &RetainedFragmentKey,
    commands: &[Command],
    hits: &[HitRegion],
    transforms: &[HitTransform],
    clips: &[HitClip],
    extents: &[ScrollOffset],
    scroll_regions: &[ScrollRegion],
    control_runs: &[ControlTextRun],
) -> Option<usize> {
    let mut bytes = 0usize;
    add_retained_bytes(&mut bytes, core::mem::size_of::<RetainedFragment>())?;
    add_retained_bytes(&mut bytes, core::mem::size_of::<RetainedFragmentKey>())?;
    add_retained_bytes(
        &mut bytes,
        key.decorations
            .len()
            .checked_mul(core::mem::size_of::<TextDecoration>())?,
    )?;
    add_retained_bytes(&mut bytes, core::mem::size_of::<Style>())?;
    add_retained_bytes(&mut bytes, core::mem::size_of::<css::StyleExtras>())?;
    add_font_spec_bytes(&mut bytes, key.parent_style.font_spec())?;
    if let Some(style) = key.style.as_ref() {
        add_retained_bytes(&mut bytes, core::mem::size_of::<Style>())?;
        add_retained_bytes(&mut bytes, core::mem::size_of::<css::StyleExtras>())?;
        add_font_spec_bytes(&mut bytes, style.font_spec())?;
    }
    for decoration in &key.decorations {
        add_font_spec_bytes(&mut bytes, &decoration.font)?;
    }
    for command in commands {
        add_retained_bytes(&mut bytes, command.referenced_bytes())?;
    }
    add_retained_bytes(
        &mut bytes,
        hits.len().checked_mul(core::mem::size_of::<HitRegion>())?,
    )?;
    add_retained_bytes(
        &mut bytes,
        transforms
            .len()
            .checked_mul(core::mem::size_of::<HitTransform>())?,
    )?;
    add_retained_bytes(
        &mut bytes,
        clips.len().checked_mul(core::mem::size_of::<HitClip>())?,
    )?;
    add_retained_bytes(
        &mut bytes,
        extents
            .len()
            .checked_mul(core::mem::size_of::<ScrollOffset>())?,
    )?;
    add_retained_bytes(
        &mut bytes,
        scroll_regions
            .len()
            .checked_mul(core::mem::size_of::<ScrollRegion>())?,
    )?;
    add_retained_bytes(
        &mut bytes,
        control_runs
            .len()
            .checked_mul(core::mem::size_of::<ControlTextRun>())?,
    )?;
    for clip in clips {
        if clip.corners.is_some() {
            add_retained_bytes(&mut bytes, core::mem::size_of::<[[f32; 2]; 4]>())?;
            add_retained_bytes(&mut bytes, 2 * core::mem::size_of::<usize>())?;
        }
    }
    for run in control_runs {
        add_font_spec_bytes(&mut bytes, &run.font)?;
    }
    // Each retained slice owns one Arc allocation; charge its header as well
    // as the payload. The style/font Arcs above are charged by reference.
    let arc_header = 2 * core::mem::size_of::<usize>();
    for _ in 0..7 {
        add_retained_bytes(&mut bytes, arc_header)?;
    }
    Some(bytes)
}

fn add_retained_bytes(bytes: &mut usize, amount: usize) -> Option<()> {
    *bytes = lumen_common::limits::size::sum(*bytes, amount, MAX_RETAINED_FRAGMENT_BYTES).ok()?;
    Some(())
}

fn add_font_spec_bytes(bytes: &mut usize, font: &FontSpec) -> Option<()> {
    let Some(families) = font.families.as_ref() else {
        return Some(());
    };
    let arc_header = 2 * core::mem::size_of::<usize>();
    add_retained_bytes(
        bytes,
        arc_header.checked_add(
            families
                .len()
                .checked_mul(core::mem::size_of::<Arc<str>>())?,
        )?,
    )?;
    for family in families.iter() {
        add_retained_bytes(bytes, family.len().checked_add(arc_header)?)?;
    }
    Some(())
}

#[derive(Clone, Copy)]
struct CollapsedBorderCandidate {
    width: f32,
    color: Rgba,
    style: BorderStyle,
    /// Cell, row, row group, and table origins in descending precedence.
    origin_rank: u8,
    /// Source order is top-to-bottom for horizontal lines and follows table
    /// track order for vertical lines (right-to-left in RTL tables).
    tie_order: usize,
    prefer_larger_tie: bool,
}

struct TableRowGroup {
    first_row: usize,
    end_row: usize,
    style: Style,
}

struct TableColumnSpan {
    first_column: usize,
    end_column: usize,
    style: Style,
    origin_rank: u8,
}

fn collapsed_border_widths(style: &Style) -> [f32; 4] {
    border_widths(style)
}

fn offer_collapsed_border_side(
    output: &mut [Option<CollapsedBorderCandidate>],
    style: &Style,
    side: usize,
    origin_rank: u8,
    tie_order: usize,
    _vertical: bool,
    _rtl: bool,
    offered: &mut usize,
) -> Result<(), LayoutError> {
    let border_style = style.border_styles()[side];
    if border_style == BorderStyle::None {
        return Ok(());
    }
    // `hidden` must take part in conflicts even though it paints and occupies
    // no border width. Other styles use the computed width after style:none
    // has reduced it to zero.
    let width = if border_style == BorderStyle::Hidden {
        0.0
    } else {
        collapsed_border_widths(style)[side]
    };
    if width < 0.0 || !width.is_finite() || (width == 0.0 && border_style != BorderStyle::Hidden) {
        return Ok(());
    }
    let colors: [Rgba; 4] =
        core::array::from_fn(|index| style.border_color_sides[index].unwrap_or(style.border_color));
    let candidate = CollapsedBorderCandidate {
        width,
        color: colors[side],
        style: border_style,
        origin_rank,
        tie_order,
        prefer_larger_tie: false,
    };
    for slot in output {
        *offered = offered.saturating_add(1);
        if *offered > MAX_COLLAPSED_BORDER_CANDIDATES {
            return Err(LayoutError::CommandLimit);
        }
        if slot.map_or(true, |current| collapsed_border_wins(candidate, current)) {
            *slot = Some(candidate);
        }
    }
    Ok(())
}

fn offer_collapsed_border_rect(
    output: &mut [Option<CollapsedBorderCandidate>],
    style: &Style,
    sides: &[usize],
    origin_rank: u8,
    tie_order: usize,
    vertical: bool,
    rtl: bool,
    offered: &mut usize,
) -> Result<(), LayoutError> {
    for &side in sides {
        offer_collapsed_border_side(
            output,
            style,
            side,
            origin_rank,
            tie_order,
            vertical,
            rtl,
            offered,
        )?;
    }
    Ok(())
}

fn empty_collapsed_edges(
    lines: usize,
    segments: usize,
) -> Result<Vec<Vec<Option<CollapsedBorderCandidate>>>, LayoutError> {
    let mut edges = Vec::new();
    edges
        .try_reserve_exact(lines)
        .map_err(|_| LayoutError::CommandLimit)?;
    for _ in 0..lines {
        let mut line = Vec::new();
        line.try_reserve_exact(segments)
            .map_err(|_| LayoutError::CommandLimit)?;
        line.resize(segments, None);
        edges.push(line);
    }
    Ok(edges)
}

fn collapsed_edge_width(
    edges: &[Vec<Option<CollapsedBorderCandidate>>],
    boundary: usize,
    start: usize,
    end: usize,
) -> f32 {
    edges
        .get(boundary)
        .into_iter()
        .flat_map(|line| line.get(start..end).unwrap_or(&[]))
        .filter_map(|candidate| *candidate)
        .map(|candidate| candidate.width)
        .fold(0.0f32, f32::max)
}

fn collapsed_border_wins(
    candidate: CollapsedBorderCandidate,
    current: CollapsedBorderCandidate,
) -> bool {
    if (candidate.style == BorderStyle::Hidden) != (current.style == BorderStyle::Hidden) {
        return candidate.style == BorderStyle::Hidden;
    }
    candidate
        .width
        .total_cmp(&current.width)
        .then_with(|| candidate.style.rank().cmp(&current.style.rank()))
        .then_with(|| candidate.origin_rank.cmp(&current.origin_rank))
        .then_with(|| {
            if candidate.prefer_larger_tie {
                candidate.tie_order.cmp(&current.tie_order)
            } else {
                current.tie_order.cmp(&candidate.tie_order)
            }
        })
        .is_gt()
}

fn replace_command_bytes(
    current: &mut Command,
    total: &mut usize,
    replacement: Command,
) -> Result<(), LayoutError> {
    let retained = total.saturating_sub(current.referenced_bytes());
    let updated = lumen_common::limits::size::sum(
        retained,
        replacement.referenced_bytes(),
        MAX_DISPLAY_LIST_BYTES,
    )
    .map_err(|_| LayoutError::CommandLimit)?;
    *current = replacement;
    *total = updated;
    Ok(())
}

fn collapsed_text(value: &str) -> Cow<'_, str> {
    let mut previous_space = false;
    let needs_collapse = value.chars().any(|ch| {
        let space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');
        let changed = space && (previous_space || ch != ' ');
        previous_space = space;
        changed
    });
    if !needs_collapse {
        return Cow::Borrowed(value);
    }
    let mut out = alloc::string::String::with_capacity(value.len());
    previous_space = false;
    for ch in value.chars() {
        let space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');
        if !space || !previous_space {
            out.push(if space { ' ' } else { ch });
        }
        previous_space = space;
    }
    Cow::Owned(out)
}

fn formatted_text(value: &str, whitespace: WhiteSpace) -> Cow<'_, str> {
    if matches!(
        whitespace,
        WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces
    ) {
        return Cow::Borrowed(value);
    }
    if whitespace != WhiteSpace::PreLine {
        return collapsed_text(value);
    }
    if !value.contains(['\t', '\r', '\x0c']) && !value.contains("  ") {
        return Cow::Borrowed(value);
    }
    let mut out = alloc::string::String::with_capacity(value.len());
    let mut space = false;
    for ch in value.chars() {
        if ch == '\n' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(ch);
            space = true;
        } else if matches!(ch, ' ' | '\t' | '\r' | '\x0c') {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(ch);
            space = false;
        }
    }
    Cow::Owned(out)
}

fn append_bounded_control_text(value: &mut String, part: &str) -> Result<(), LayoutError> {
    let remaining = MAX_CONTROL_TEXT_BYTES.saturating_sub(value.len());
    if part.len() > remaining {
        return Err(LayoutError::CommandLimit);
    }
    value
        .try_reserve(part.len())
        .map_err(|_| LayoutError::CommandLimit)?;
    value.push_str(part);
    Ok(())
}

fn control_grapheme_ranges(value: &str) -> Result<Vec<Range<usize>>, LayoutError> {
    let count = lumen_common::ucd::graphemes(value).count();
    let mut ranges = Vec::new();
    ranges
        .try_reserve(count)
        .map_err(|_| LayoutError::CommandLimit)?;
    for (start, grapheme) in lumen_common::ucd::graphemes(value) {
        ranges.push(start..start + grapheme.len());
    }
    Ok(ranges)
}

fn control_source_range(
    display_range: &Range<usize>,
    source_graphemes: Option<&[Range<usize>]>,
    placeholder: bool,
) -> Result<Range<usize>, LayoutError> {
    if placeholder {
        return Ok(0..0);
    }
    let Some(source_graphemes) = source_graphemes else {
        return Ok(display_range.clone());
    };
    const BULLET_BYTES: usize = '\u{2022}'.len_utf8();
    if display_range.start % BULLET_BYTES != 0 || display_range.end % BULLET_BYTES != 0 {
        return Err(LayoutError::Text);
    }
    let first = display_range.start / BULLET_BYTES;
    let end = display_range.end / BULLET_BYTES;
    if first > end || end > source_graphemes.len() {
        return Err(LayoutError::Text);
    }
    if first == end {
        let offset = source_graphemes.get(first).map_or_else(
            || source_graphemes.last().map_or(0, |range| range.end),
            |range| range.start,
        );
        return Ok(offset..offset);
    }
    Ok(source_graphemes[first].start..source_graphemes[end - 1].end)
}

pub(crate) fn stylesheets(document: &Document) -> Result<StyleIndex, LayoutError> {
    stylesheets_with_linked(document, &[])
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StylesheetIdentity {
    Link(String),
    Inline(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LoadedStylesheet {
    pub owner: NodeId,
    pub identity: StylesheetIdentity,
    pub source: css::StylesheetSource,
}

fn assign_css_font_face_identities(
    parsed: &mut css::ParsedStylesheets,
    owner: css::FontFaceOwnerId,
) {
    for face in &mut parsed.font_faces {
        face.identity = Some(css::FontFaceIdentity::Css(css::FontFaceRuleId {
            owner: owner.clone(),
            sheet_revision: face.source_revision,
            source_url: face.source_url.clone(),
            import_path: face.import_path.clone(),
            rule_start: face.rule_start,
        }));
    }
}

pub(crate) fn stylesheet_identity(
    document: &Document,
    owner: NodeId,
) -> Result<Option<StylesheetIdentity>, LayoutError> {
    let NodeKind::Element {
        name,
        namespace,
        attributes,
    } = document.kind(owner).map_err(|_| LayoutError::InvalidTree)?
    else {
        return Ok(None);
    };
    if !matches!(namespace, Namespace::Html | Namespace::Svg) {
        return Ok(None);
    }
    let tag = crate::svg::local_name(name);
    if *namespace == Namespace::Html && tag == "link" {
        return Ok(Some(StylesheetIdentity::Link(
            crate::svg::attribute(attributes, "href")
                .unwrap_or_default()
                .into(),
        )));
    }
    if tag != "style" {
        return Ok(None);
    }
    let mut text = String::new();
    let mut child = document
        .first_child(owner)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        if let NodeKind::Text(value) = document
            .kind(current)
            .map_err(|_| LayoutError::InvalidTree)?
        {
            text.push_str(value);
        }
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    Ok(Some(StylesheetIdentity::Inline(text)))
}

pub(crate) fn stylesheets_with_linked(
    document: &Document,
    linked: &[(NodeId, alloc::string::String, alloc::string::String)],
) -> Result<StyleIndex, LayoutError> {
    stylesheets_with_sources(
        document,
        linked,
        &[],
        &[],
        None,
        css::MediaEnvironment::default(),
    )
    .map(|(rules, _)| rules)
}

pub(crate) fn stylesheets_with_sources(
    document: &Document,
    linked: &[(NodeId, String, String)],
    sources: &[LoadedStylesheet],
    adopted: &[(Option<NodeId>, Vec<String>)],
    document_base: Option<Arc<str>>,
    environment: css::MediaEnvironment,
) -> Result<(StyleIndex, Vec<css::FontFaceRule>), LayoutError> {
    let mut rules = Vec::new();
    let mut sheets = Vec::new();
    let mut pending = alloc::vec![document.root()];
    while let Some(id) = pending.pop() {
        if let NodeKind::Element {
            name,
            namespace,
            attributes,
        } = document.kind(id).map_err(|_| LayoutError::InvalidTree)?
        {
            if !matches!(namespace, Namespace::Html | Namespace::Svg) {
                continue;
            }
            let tag = crate::svg::local_name(name);
            if *namespace == Namespace::Html && tag == "template" {
                continue;
            }
            if *namespace == Namespace::Html && tag == "link" {
                let attribute = |key: &str| crate::svg::attribute(attributes, key);
                let rel = attribute("rel").unwrap_or("");
                let stylesheet = rel
                    .split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case("stylesheet"));
                let alternate = rel
                    .split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case("alternate"));
                let css_type = attribute("type")
                    .is_none_or(|value| value.is_empty() || value.eq_ignore_ascii_case("text/css"));
                if stylesheet && !alternate && css_type && attribute("disabled").is_none() {
                    if let Some((_, _, text)) = linked.iter().find(|(owner, href, _)| {
                        *owner == id && attribute("href") == Some(href.as_str())
                    }) {
                        let mut parsed = if let Some(source) = sources.iter().find(|source| {
                            source.owner == id
                                && source.identity
                                    == StylesheetIdentity::Link(
                                        attribute("href").unwrap_or("").into(),
                                    )
                                && source.source.text.as_ref() == text
                        }) {
                            css::parse_graph(&source.source, environment)
                                .map_err(LayoutError::Css)?
                        } else {
                            css::parse_stylesheet(text).map_err(LayoutError::Css)?
                        };
                        if let Some(media) =
                            attribute("media").filter(|value| !value.trim().is_empty())
                        {
                            for rule in &mut parsed.rules {
                                let mut conditions = rule.media.to_vec();
                                conditions.push(Arc::from(media));
                                rule.media = conditions.into();
                            }
                            for face in &mut parsed.font_faces {
                                let mut conditions = face.media.to_vec();
                                conditions.push(Arc::from(media));
                                face.media = conditions.into();
                            }
                        }
                        assign_css_font_face_identities(
                            &mut parsed,
                            css::FontFaceOwnerId::Element(id),
                        );
                        if rules.len() + parsed.rules.len() > css::MAX_RULES {
                            return Err(LayoutError::Css(css::CssError {
                                offset: 0,
                                message: "too many rules",
                            }));
                        }
                        rules.extend(parsed.rules.iter().cloned());
                        sheets.push(parsed);
                    }
                }
                continue;
            }
            if tag == "style" {
                let css_type = crate::svg::attribute(attributes, "type")
                    .is_none_or(|value| value.is_empty() || value.eq_ignore_ascii_case("text/css"));
                if !css_type {
                    continue;
                }
                let identity =
                    stylesheet_identity(document, id)?.ok_or(LayoutError::InvalidTree)?;
                let StylesheetIdentity::Inline(text) = &identity else {
                    return Err(LayoutError::InvalidTree);
                };
                let mut parsed = if let Some(source) = sources
                    .iter()
                    .find(|source| source.owner == id && source.identity == identity)
                {
                    css::parse_graph(&source.source, environment).map_err(LayoutError::Css)?
                } else {
                    css::parse_stylesheet(text).map_err(LayoutError::Css)?
                };
                if let Some(media) = crate::svg::attribute(attributes, "media") {
                    if !media.trim().is_empty() {
                        for rule in &mut parsed.rules {
                            let mut conditions = rule.media.to_vec();
                            conditions.push(Arc::from(media));
                            rule.media = conditions.into();
                        }
                        for face in &mut parsed.font_faces {
                            let mut conditions = face.media.to_vec();
                            conditions.push(Arc::from(media));
                            face.media = conditions.into();
                        }
                    }
                }
                assign_css_font_face_identities(&mut parsed, css::FontFaceOwnerId::Element(id));
                if rules.len() + parsed.rules.len() > css::MAX_RULES {
                    return Err(LayoutError::Css(css::CssError {
                        offset: 0,
                        message: "too many rules",
                    }));
                }
                rules.extend(parsed.rules.iter().cloned());
                sheets.push(parsed);
                continue;
            }
        }
        let mut child = document
            .last_child(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = child {
            pending.push(current);
            child = document
                .previous_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
    }
    // Shadow tree stylesheets are scoped: their rules apply only inside their
    // own tree (plus `:host`/`::slotted()` reaching outward).
    for (_, root, _) in document.shadow_roots() {
        let mut pending = alloc::vec![root];
        while let Some(id) = pending.pop() {
            let recurse = match document.kind(id).map_err(|_| LayoutError::InvalidTree)? {
                NodeKind::DocumentFragment => true,
                NodeKind::Element {
                    name, namespace, ..
                } if matches!(namespace, Namespace::Html | Namespace::Svg)
                    && !(*namespace == Namespace::Html
                        && crate::svg::local_name(name) == "template") =>
                {
                    if crate::svg::local_name(name) == "style" {
                        let mut text = alloc::string::String::new();
                        let mut child = document
                            .first_child(id)
                            .map_err(|_| LayoutError::InvalidTree)?;
                        while let Some(current) = child {
                            if let NodeKind::Text(value) = document
                                .kind(current)
                                .map_err(|_| LayoutError::InvalidTree)?
                            {
                                text.push_str(value);
                            }
                            child = document
                                .next_sibling(current)
                                .map_err(|_| LayoutError::InvalidTree)?;
                        }
                        let parsed =
                            css::parse_scoped(&text, Some(root)).map_err(LayoutError::Css)?;
                        if rules.len() + parsed.len() > css::MAX_RULES {
                            return Err(LayoutError::Css(css::CssError {
                                offset: 0,
                                message: "too many rules",
                            }));
                        }
                        rules.extend(parsed);
                        false
                    } else {
                        true
                    }
                }
                _ => false,
            };
            if recurse {
                let mut child = document
                    .last_child(id)
                    .map_err(|_| LayoutError::InvalidTree)?;
                while let Some(current) = child {
                    pending.push(current);
                    child = document
                        .previous_sibling(current)
                        .map_err(|_| LayoutError::InvalidTree)?;
                }
            }
        }
    }
    for (scope, adopted_sheets) in adopted {
        let mut adopted_revisions = alloc::collections::BTreeMap::<u64, usize>::new();
        for text in adopted_sheets {
            let mut parsed = css::parse_stylesheet(text).map_err(LayoutError::Css)?;
            let revision = css::stylesheet_source_revision(text);
            let occurrence = adopted_revisions.entry(revision).or_default();
            let duplicate_index = *occurrence;
            *occurrence += 1;
            assign_css_font_face_identities(
                &mut parsed,
                css::FontFaceOwnerId::Adopted {
                    scope: *scope,
                    stylesheet: revision,
                    duplicate_index,
                },
            );
            if rules.len() + parsed.rules.len() > css::MAX_RULES {
                return Err(LayoutError::Css(css::CssError {
                    offset: 0,
                    message: "too many rules",
                }));
            }
            for rule in &mut parsed.rules {
                rule.scope = *scope;
            }
            rules.extend(parsed.rules.iter().cloned());
            // Font selection currently has document scope. Do not leak a
            // shadow tree's named font definitions into the document set.
            if scope.is_none() {
                sheets.push(parsed);
            }
        }
    }
    let faces = css::canonicalize_font_faces(&mut sheets)
        .into_iter()
        .map(|mut face| {
            if face.source_url.is_none() {
                face.source_url = document_base.clone();
            }
            if let Some(css::FontFaceIdentity::Css(mut identity)) = face.identity.take() {
                identity.source_url = face.source_url.clone();
                face.identity = Some(css::FontFaceIdentity::Css(identity));
            }
            face
        })
        .collect();
    Ok((
        StyleIndex::new_with_document_base_url(rules, document_base),
        faces,
    ))
}

struct Layout<'a> {
    document: &'a Document,
    text: &'a dyn TextShaper,
    rules: &'a StyleIndex,
    images: Option<&'a dyn ImageResolver>,
    commands: Vec<Command>,
    command_bytes: usize,
    body_background_on_canvas: bool,
    viewport: Rect,
    geometry: Option<&'a mut LayoutGeometry>,
    scrolls: &'a [ScrollOffset],
    /// Region whose content is emitted; the viewport, widened to a scroll
    /// window inside overflow clips so small scrolls can be replayed by
    /// translation.
    cull: Rect,
    /// Bumped by content whose position does not follow its scroll container
    /// uniformly (out-of-flow, sticky, multicol); such scrollers relayout.
    scroll_hazards: usize,
    containing_block: Option<Rect>,
    fixed_containing_block: Option<Rect>,
    sticky_bounds: Option<Rect>,
    transform_depth: usize,
    parent_height: Option<f32>,
    decorations: Vec<TextDecoration>,
    /// Effective collapsed bottom margin of the last laid out container
    /// (CSS 2.1 §8.3.1), read by the parent walk right after `box_for`.
    collapsed_bottom: Option<f32>,
    /// Set when a container collapsed through completely; the parent walk
    /// then treats the child as transparent and materializes the chain once.
    was_through: Option<f32>,
    /// Insertion points (command index, hit index) of enclosing stacking
    /// contexts while the walk descends (CSS 2.1 Appendix E).
    stacking_roots: Vec<(usize, usize)>,
    /// Negative-z subtrees laid out inside non-stacking-context containers,
    /// waiting to be rotated below the backgrounds of their stacking context:
    /// (target command, target hit, first cmd, last cmd, first hit, last hit).
    pending_escapes: Vec<(usize, usize, usize, usize, usize, usize)>,
    /// In-flow `position: relative; z-index: auto` subtrees, kept with the
    /// nearest block walk so later positioned siblings can retain tree order.
    positioned_flow_paints: Vec<PositionedFlowPaint>,
    next_paint_order: u64,
    /// Subtree intrinsic sizes keyed by node index and minimum mode; an entry
    /// is reused only when the style it was measured with is unchanged.
    intrinsic_cache: core::cell::RefCell<Vec<Option<(Style, (f32, f32))>>>,
    /// Cascade results for this run, keyed by node and parent style.
    style_cache: &'a core::cell::RefCell<css::StyleCache>,
    /// Session-owned bounded subtree command/geometry cache.
    retained_fragments: Option<&'a mut RetainedLayoutCache>,
    /// Cell borders are painted once by the collapsed-border conflict pass.
    suppressed_border_node: Option<NodeId>,
}

#[derive(Clone, Debug, PartialEq)]
struct PositionedFlowPaint {
    order: u64,
    commands: Range<usize>,
    hits: Range<usize>,
}

fn is_svg_root_element(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Element {
            name,
            namespace: Namespace::Svg,
            ..
        } if crate::svg::local_name(name) == "svg"
    )
}

impl Layout<'_> {
    fn computed_style(&self, node: NodeId, parent: Option<&Style>) -> Result<Style, css::CssError> {
        css::compute_node_cached_with_text(
            self.document,
            node,
            parent,
            self.rules,
            &mut self.style_cache.borrow_mut(),
            self.text,
        )
    }

    fn find_svg_id(&self, id: &str) -> Result<Option<NodeId>, LayoutError> {
        svg_element_with_id(self.document, id)
    }

    fn computed_style_from_tree(&self, node: NodeId) -> Result<Style, LayoutError> {
        let mut ancestors = Vec::new();
        let mut current = Some(node);
        for _ in 0..=512 {
            let Some(id) = current else {
                break;
            };
            ancestors.push(id);
            current = self
                .document
                .parent(id)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        if current.is_some() {
            return Err(LayoutError::DepthLimit);
        }
        let mut parent_style = None;
        for ancestor in ancestors.into_iter().rev() {
            if matches!(self.document.kind(ancestor), Ok(NodeKind::Element { .. })) {
                let style = self
                    .computed_style(ancestor, parent_style.as_ref())
                    .map_err(LayoutError::Css)?;
                parent_style = Some(style);
            }
        }
        parent_style.ok_or(LayoutError::InvalidTree)
    }

    fn svg_href(&self, node: NodeId) -> Option<String> {
        self.document
            .get_attribute_ns(node, None, "href")
            .ok()
            .flatten()
            .or_else(|| {
                self.document
                    .get_attribute_ns(node, Some("http://www.w3.org/1999/xlink"), "href")
                    .ok()
                    .flatten()
            })
    }

    fn svg_gradient(
        &self,
        reference: &str,
        user_box: crate::svg::ViewBox,
    ) -> Result<Option<Arc<crate::paint::SvgGradient>>, LayoutError> {
        let Some(id) = reference.strip_prefix('#') else {
            return Ok(None);
        };
        let Some(node) = self.find_svg_id(id)? else {
            return Ok(None);
        };
        self.svg_gradient_node(node, user_box, &mut Vec::new(), 0)
    }

    fn svg_gradient_node(
        &self,
        node: NodeId,
        user_box: crate::svg::ViewBox,
        seen: &mut Vec<NodeId>,
        depth: usize,
    ) -> Result<Option<Arc<crate::paint::SvgGradient>>, LayoutError> {
        if depth >= 32 || seen.contains(&node) {
            return Ok(None);
        }
        let NodeKind::Element {
            namespace: Namespace::Svg,
            name,
            attributes,
        } = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Ok(None);
        };
        let tag = crate::svg::local_name(name);
        let linear = tag == "linearGradient";
        if !linear && tag != "radialGradient" {
            return Ok(None);
        }
        seen.push(node);
        let mut inherited = None;
        if let Some(href) = self.svg_href(node) {
            if let Some(parent_id) = href.strip_prefix('#') {
                if let Some(parent_node) = self.find_svg_id(parent_id)? {
                    if parent_node == node || seen.contains(&parent_node) {
                        seen.pop();
                        return Ok(None);
                    }
                    if let Some(gradient) =
                        self.svg_gradient_node(parent_node, user_box, seen, depth + 1)?
                    {
                        if matches!(
                            (&gradient.kind, linear),
                            (crate::paint::SvgGradientKind::Linear { .. }, true)
                                | (crate::paint::SvgGradientKind::Radial { .. }, false)
                        ) {
                            inherited = Some(gradient);
                        }
                    }
                }
            }
        }
        seen.pop();
        let inherited = inherited.as_deref();
        let units = match crate::svg::attribute(attributes, "gradientUnits") {
            Some("userSpaceOnUse") => crate::paint::SvgGradientUnits::UserSpaceOnUse,
            Some("objectBoundingBox") => crate::paint::SvgGradientUnits::ObjectBoundingBox,
            Some(_) => return Ok(None),
            None => inherited.map_or(
                crate::paint::SvgGradientUnits::ObjectBoundingBox,
                |gradient| gradient.units,
            ),
        };
        if crate::svg::attribute(attributes, "spreadMethod").is_some_and(|method| method != "pad") {
            return Ok(None);
        }
        let transform = match crate::svg::attribute(attributes, "gradientTransform") {
            Some(value) => match crate::svg::parse_transform(value) {
                Some(transform) if transform.inverse().is_some() => transform,
                None => return Ok(None),
                Some(_) => return Ok(None),
            },
            None => inherited.map_or(crate::paint::Affine::IDENTITY, |gradient| {
                gradient.transform
            }),
        };
        let coordinate = |key: &str, axis: usize, default: f32| -> Option<f32> {
            let Some(value) = crate::svg::attribute(attributes, key) else {
                return Some(default);
            };
            parse_svg_gradient_coordinate(value, units, axis, user_box)
        };
        let kind = if linear {
            let default_start = match units {
                crate::paint::SvgGradientUnits::ObjectBoundingBox => [0.0, 0.0],
                crate::paint::SvgGradientUnits::UserSpaceOnUse => [user_box.min_x, user_box.min_y],
            };
            let default_end = match units {
                crate::paint::SvgGradientUnits::ObjectBoundingBox => [1.0, 0.0],
                crate::paint::SvgGradientUnits::UserSpaceOnUse => {
                    [user_box.min_x + user_box.width, user_box.min_y]
                }
            };
            let inherited_coords = inherited.and_then(|gradient| match gradient.kind {
                crate::paint::SvgGradientKind::Linear { start, end } => Some((start, end)),
                _ => None,
            });
            let defaults = inherited_coords.unwrap_or((default_start, default_end));
            let Some(x1) = coordinate("x1", 0, defaults.0[0]) else {
                return Ok(None);
            };
            let Some(y1) = coordinate("y1", 1, defaults.0[1]) else {
                return Ok(None);
            };
            let Some(x2) = coordinate("x2", 0, defaults.1[0]) else {
                return Ok(None);
            };
            let Some(y2) = coordinate("y2", 1, defaults.1[1]) else {
                return Ok(None);
            };
            let start = [x1, y1];
            let end = [x2, y2];
            crate::paint::SvgGradientKind::Linear { start, end }
        } else {
            let default_center = match units {
                crate::paint::SvgGradientUnits::ObjectBoundingBox => [0.5, 0.5],
                crate::paint::SvgGradientUnits::UserSpaceOnUse => [
                    user_box.min_x + user_box.width * 0.5,
                    user_box.min_y + user_box.height * 0.5,
                ],
            };
            let default_radius = match units {
                crate::paint::SvgGradientUnits::ObjectBoundingBox => 0.5,
                crate::paint::SvgGradientUnits::UserSpaceOnUse => {
                    ((user_box.width * user_box.width + user_box.height * user_box.height) * 0.5)
                        .sqrt()
                        * 0.5
                }
            };
            let inherited_coords = inherited.and_then(|gradient| match gradient.kind {
                crate::paint::SvgGradientKind::Radial {
                    start,
                    end,
                    start_radius,
                    end_radius,
                } => Some((start, end, start_radius, end_radius)),
                _ => None,
            });
            let defaults =
                inherited_coords.unwrap_or((default_center, default_center, 0.0, default_radius));
            let Some(cx) = coordinate("cx", 0, defaults.1[0]) else {
                return Ok(None);
            };
            let Some(cy) = coordinate("cy", 1, defaults.1[1]) else {
                return Ok(None);
            };
            let Some(fx) = coordinate("fx", 0, defaults.0[0]) else {
                return Ok(None);
            };
            let Some(fy) = coordinate("fy", 1, defaults.0[1]) else {
                return Ok(None);
            };
            let end_radius = match crate::svg::attribute(attributes, "r") {
                Some(value) => {
                    let Some(radius) = parse_svg_gradient_radius(value, units, user_box) else {
                        return Ok(None);
                    };
                    radius
                }
                None => defaults.3,
            };
            let start_radius = match crate::svg::attribute(attributes, "fr") {
                Some(value) => {
                    let Some(radius) = parse_svg_gradient_radius(value, units, user_box) else {
                        return Ok(None);
                    };
                    radius
                }
                None => defaults.2,
            };
            if start_radius < 0.0 || end_radius < 0.0 {
                return Ok(None);
            }
            crate::paint::SvgGradientKind::Radial {
                start: [fx, fy],
                end: [cx, cy],
                start_radius,
                end_radius,
            }
        };
        match kind {
            crate::paint::SvgGradientKind::Linear { start, end } if start == end => {
                return Ok(None);
            }
            crate::paint::SvgGradientKind::Radial { end_radius, .. } if end_radius <= 0.0 => {
                return Ok(None);
            }
            _ => {}
        }

        let style = self.computed_style_from_tree(node)?;
        let mut stops = Vec::new();
        let mut child = self
            .document
            .first_child(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        let mut last_offset = 0.0f32;
        while let Some(stop) = child {
            child = self
                .document
                .next_sibling(stop)
                .map_err(|_| LayoutError::InvalidTree)?;
            let NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                attributes,
            } = self
                .document
                .kind(stop)
                .map_err(|_| LayoutError::InvalidTree)?
            else {
                continue;
            };
            if crate::svg::local_name(name) != "stop" {
                continue;
            }
            if stops.len() >= crate::paint::MAX_SVG_GRADIENT_STOPS {
                return Ok(None);
            }
            let stop_style = self
                .computed_style(stop, Some(&style))
                .map_err(LayoutError::Css)?;
            let raw_offset = crate::svg::attribute(attributes, "offset")
                .and_then(parse_svg_stop_offset)
                .unwrap_or(0.0);
            let offset = raw_offset.clamp(last_offset, 1.0);
            last_offset = offset;
            let mut color = stop_style.svg_stop_color;
            color.a = (f32::from(color.a) * stop_style.svg_stop_opacity)
                .round()
                .clamp(0.0, 255.0) as u8;
            stops.push(crate::paint::SvgGradientStop { offset, color });
        }
        let stops: Arc<[crate::paint::SvgGradientStop]> = if stops.is_empty() {
            inherited.map_or_else(
                || Arc::from(Vec::<crate::paint::SvgGradientStop>::new()),
                |gradient| gradient.stops.clone(),
            )
        } else if stops.len() == 1 {
            let color = stops[0].color;
            Arc::from([
                crate::paint::SvgGradientStop { offset: 0.0, color },
                crate::paint::SvgGradientStop { offset: 1.0, color },
            ])
        } else {
            stops.into()
        };
        if stops.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(crate::paint::SvgGradient {
            units,
            transform,
            kind,
            stops,
        })))
    }

    fn svg_clip(
        &self,
        reference: &str,
        user_box: crate::svg::ViewBox,
    ) -> Result<Option<Arc<crate::paint::SvgClip>>, LayoutError> {
        let Some(id) = reference.strip_prefix('#') else {
            return Ok(None);
        };
        let Some(node) = self.find_svg_id(id)? else {
            return Ok(None);
        };
        let NodeKind::Element {
            namespace: Namespace::Svg,
            name,
            attributes,
        } = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Ok(None);
        };
        if crate::svg::local_name(name) != "clipPath" {
            return Ok(None);
        }
        let units = match crate::svg::attribute(attributes, "clipPathUnits") {
            Some("objectBoundingBox") => crate::paint::SvgGradientUnits::ObjectBoundingBox,
            Some("userSpaceOnUse") | None => crate::paint::SvgGradientUnits::UserSpaceOnUse,
            Some(_) => return Ok(None),
        };
        let transform = match crate::svg::attribute(attributes, "transform") {
            Some(value) => match crate::svg::parse_transform(value) {
                Some(transform) => transform,
                None => return Ok(None),
            },
            None => Affine::IDENTITY,
        };
        let mut shapes = Vec::new();
        let clip_user_box = if units == crate::paint::SvgGradientUnits::ObjectBoundingBox {
            crate::svg::ViewBox {
                min_x: 0.0,
                min_y: 0.0,
                width: 1.0,
                height: 1.0,
            }
        } else {
            user_box
        };
        self.collect_svg_clip_shapes(node, Affine::IDENTITY, clip_user_box, &mut shapes, 0)?;
        if shapes.len() > crate::paint::MAX_SVG_CLIP_PATHS {
            return Ok(None);
        }
        Ok(Some(Arc::new(crate::paint::SvgClip {
            units,
            transform,
            shapes: shapes.into(),
        })))
    }

    fn collect_svg_clip_shapes(
        &self,
        parent: NodeId,
        parent_transform: Affine,
        user_box: crate::svg::ViewBox,
        output: &mut Vec<crate::paint::SvgClipShape>,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if depth > 32 || output.len() > crate::paint::MAX_SVG_CLIP_PATHS {
            return Ok(());
        }
        let mut child = self
            .document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                attributes,
            } = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?
            else {
                continue;
            };
            let tag = crate::svg::local_name(name);
            let style = self.computed_style_from_tree(node)?;
            if style.display == Display::None {
                continue;
            }
            let local = crate::svg::attribute(attributes, "transform")
                .and_then(crate::svg::parse_transform)
                .unwrap_or(Affine::IDENTITY);
            let transform = parent_transform.then(local);
            if matches!(tag, "g" | "a") {
                self.collect_svg_clip_shapes(node, transform, user_box, output, depth + 1)?;
                continue;
            }
            let path = if tag == "path" {
                crate::svg::attribute(attributes, "d")
                    .filter(|data| data.len() <= crate::paint::MAX_SVG_PATH_BYTES)
                    .map(String::from)
            } else {
                crate::svg::shape_path_with_geometry(
                    tag,
                    attributes,
                    user_box,
                    &style.svg_geometry,
                    style.width,
                    style.height,
                )
            };
            let Some(path) = path.filter(|path| !path.is_empty()) else {
                continue;
            };
            output.push(crate::paint::SvgClipShape {
                data: Arc::from(path),
                transform,
                fill_rule: match style.svg_clip_rule {
                    css::SvgFillRule::NonZero => crate::paint::SvgFillRule::NonZero,
                    css::SvgFillRule::EvenOdd => crate::paint::SvgFillRule::EvenOdd,
                },
            });
        }
        Ok(())
    }

    fn paint_svg_shape(
        &mut self,
        tag: &str,
        attributes: &[(crate::Name, String)],
        style: &Style,
        transform: Affine,
        viewport: Rect,
        inherited_opacity: f32,
        user_box: crate::svg::ViewBox,
        clips: &[Arc<crate::paint::SvgClip>],
    ) -> Result<(), LayoutError> {
        let path = if tag == "path" {
            crate::svg::attribute(attributes, "d")
                .filter(|data| data.len() <= crate::paint::MAX_SVG_PATH_BYTES)
                .map(String::from)
        } else {
            crate::svg::shape_path_with_geometry(
                tag,
                attributes,
                user_box,
                &style.svg_geometry,
                style.width,
                style.height,
            )
        };
        let Some(path) = path.filter(|path| !path.is_empty()) else {
            return Ok(());
        };
        if !style.visibility_visible {
            return Ok(());
        }
        let fill =
            self.resolve_svg_paint(&style.svg_fill, style.color, inherited_opacity, user_box)?;
        let stroke =
            self.resolve_svg_paint(&style.svg_stroke, style.color, inherited_opacity, user_box)?;
        if fill.is_none() && stroke.is_none() {
            return Ok(());
        }
        let opacity = style.opacity.clamp(0.0, 1.0);
        let opacity_layer = opacity < 1.0;
        if opacity_layer {
            self.push_command(Command::PushLayer {
                corners: None,
                rect: viewport,
                radius: 0.0,
                opacity,
                clip: false,
            })?;
        }
        self.push_command(Command::SvgPath {
            bounds: viewport,
            data: Arc::from(path),
            transform,
            fill,
            stroke,
            stroke_width: style.svg_stroke_width,
            fill_rule: match style.svg_fill_rule {
                css::SvgFillRule::NonZero => crate::paint::SvgFillRule::NonZero,
                css::SvgFillRule::EvenOdd => crate::paint::SvgFillRule::EvenOdd,
            },
            clips: clips.iter().map(|clip| clip.as_ref().clone()).collect(),
        })?;
        if opacity_layer {
            self.push_command(Command::PopLayer)?;
        }
        Ok(())
    }

    fn resolve_svg_paint(
        &self,
        paint: &css::SvgPaint,
        current_color: Rgba,
        opacity: f32,
        user_box: crate::svg::ViewBox,
    ) -> Result<Option<crate::paint::SvgPaint>, LayoutError> {
        let solid = |color| {
            Some(crate::paint::SvgPaint::Color(svg_color_opacity(
                color, opacity,
            )))
        };
        match paint {
            css::SvgPaint::Color(color) => Ok(solid(*color)),
            css::SvgPaint::CurrentColor => Ok(solid(current_color)),
            css::SvgPaint::None => Ok(None),
            css::SvgPaint::Unsupported(fallback) => Ok(fallback.map(solid).flatten()),
            css::SvgPaint::Reference(reference, fallback) => {
                if let Some(gradient) = self.svg_gradient(reference, user_box)? {
                    Ok(Some(crate::paint::SvgPaint::Gradient(gradient)))
                } else {
                    Ok(fallback.map(solid).flatten())
                }
            }
        }
    }

    fn paint_svg_root(
        &mut self,
        id: NodeId,
        attributes: &[(crate::Name, String)],
        style: &Style,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        let resolved_style = style.resolve_percentages(available, self.parent_height);
        let style = &resolved_style;
        let [margin_top, margin_right, margin_bottom, margin_left] = style.margin_sides;
        let [padding_top, padding_right, padding_bottom, padding_left] = style.padding_sides;
        let [border_top, border_right, border_bottom, border_left] = border_widths(style);
        let attr_width = style.width.map(|width| content_dimension(style, width));
        let attr_height = style
            .height
            .map(|height| content_height_dimension(style, height));
        let (intrinsic_width, intrinsic_height) = crate::svg::root_size(
            attributes,
            attr_width,
            attr_height,
            available,
            self.parent_height,
        );
        let content_width = constrained_width(style, intrinsic_width.max(0.0));
        let content_height = constrained_height(style, intrinsic_height.max(0.0));
        let outer_x = x + margin_left;
        let outer_y = y + margin_top;
        let outer_width = content_width + padding_left + padding_right + border_left + border_right;
        let outer_height =
            content_height + padding_top + padding_bottom + border_top + border_bottom;
        let outer = Rect {
            x: outer_x,
            y: outer_y,
            width: outer_width,
            height: outer_height,
        };
        let hit = self.begin_hit(id)?;
        self.finish_hit(hit, outer);
        let opacity_layer = style.opacity < 1.0;
        if opacity_layer {
            self.push_command(Command::PushLayer {
                corners: None,
                rect: outer,
                radius: 0.0,
                opacity: style.opacity.clamp(0.0, 1.0),
                clip: false,
            })?;
        }
        if style.visibility_visible
            && has_background(style)
            && (self.transform_depth > 0 || outer.intersection(self.cull).is_some())
        {
            self.push_background(outer, style)?;
        }
        if style.visibility_visible
            && self.suppressed_border_node != Some(id)
            && border_widths(style).iter().any(|width| *width > 0.0)
            && (self.transform_depth > 0 || outer.intersection(self.cull).is_some())
        {
            if has_nonuniform_border(style) {
                for command in side_border_commands(outer, style) {
                    self.push_command(command)?;
                }
            } else if style.border_solid && style.border_width > 0.0 {
                self.push_command(border(outer, style))?;
            }
        }
        let viewport = Rect {
            x: outer_x + border_left + padding_left,
            y: outer_y + border_top + padding_top,
            width: content_width,
            height: content_height,
        };
        if style.visibility_visible
            && style.opacity > 0.0
            && (self.transform_depth > 0 || viewport.intersection(self.cull).is_some())
            && viewport.width > 0.0
            && viewport.height > 0.0
        {
            let view_box =
                crate::svg::attribute(attributes, "viewBox").and_then(crate::svg::parse_view_box);
            let (mut transform, user_box) = crate::svg::view_box_transform(
                view_box,
                attributes,
                viewport.x,
                viewport.y,
                viewport.width,
                viewport.height,
            );
            if let Some(root_transform) =
                crate::svg::attribute(attributes, "transform").and_then(crate::svg::parse_transform)
            {
                transform = transform.then(root_transform);
            }
            self.push_command(Command::PushClip(viewport))?;
            let mut use_chain = Vec::new();
            self.paint_svg_children(
                id,
                style,
                transform,
                viewport,
                1.0,
                user_box,
                depth + 1,
                &mut use_chain,
                &[],
            )?;
            self.push_command(Command::PopClip)?;
        }
        if opacity_layer {
            self.push_command(Command::PopLayer)?;
        }
        Ok(outer_height + margin_top + margin_bottom)
    }

    fn paint_svg_children(
        &mut self,
        parent: NodeId,
        parent_style: &Style,
        transform: Affine,
        viewport: Rect,
        inherited_opacity: f32,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
        inherited_clips: &[Arc<crate::paint::SvgClip>],
    ) -> Result<(), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let mut child = self
            .document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                attributes,
            } = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?
            else {
                continue;
            };
            let tag = crate::svg::local_name(name);
            let style = self
                .computed_style(node, Some(parent_style))
                .map_err(LayoutError::Css)?;
            if style.display == Display::None {
                continue;
            }
            if matches!(
                tag,
                "defs"
                    | "symbol"
                    | "clipPath"
                    | "mask"
                    | "filter"
                    | "marker"
                    | "pattern"
                    | "linearGradient"
                    | "radialGradient"
                    | "title"
                    | "desc"
                    | "metadata"
                    | "style"
                    | "script"
                    | "foreignObject"
            ) {
                continue;
            }
            let local_transform = crate::svg::attribute(attributes, "transform")
                .and_then(crate::svg::parse_transform)
                .unwrap_or(Affine::IDENTITY);
            let node_transform = transform.then(local_transform);
            let mut node_clips = inherited_clips.to_vec();
            if let Some(reference) = style.svg_clip_path.as_deref() {
                if let Some(clip) = self.svg_clip(reference, user_box)? {
                    node_clips.push(clip);
                }
            }
            let opacity = style.opacity.clamp(0.0, 1.0);
            if tag == "g" || tag == "a" || tag == "svg" {
                if opacity == 0.0 {
                    continue;
                }
                let opacity_layer = style.opacity < 1.0;
                if opacity_layer {
                    self.push_command(Command::PushLayer {
                        corners: None,
                        rect: viewport,
                        radius: 0.0,
                        opacity: style.opacity.clamp(0.0, 1.0),
                        clip: false,
                    })?;
                }
                self.paint_svg_children(
                    node,
                    &style,
                    node_transform,
                    viewport,
                    inherited_opacity,
                    user_box,
                    depth + 1,
                    use_chain,
                    &node_clips,
                )?;
                if opacity_layer {
                    self.push_command(Command::PopLayer)?;
                }
                continue;
            }
            if tag == "use" {
                self.paint_svg_use(
                    node,
                    &style,
                    attributes,
                    node_transform,
                    viewport,
                    inherited_opacity,
                    user_box,
                    depth + 1,
                    use_chain,
                    &node_clips,
                )?;
                continue;
            }
            if tag == "text" {
                if style.visibility_visible {
                    let opacity_layer = opacity < 1.0;
                    if opacity_layer {
                        self.push_command(Command::PushLayer {
                            corners: None,
                            rect: viewport,
                            radius: 0.0,
                            opacity,
                            clip: false,
                        })?;
                    }
                    let mut text = String::new();
                    self.collect_svg_text(node, &mut text, depth + 1)?;
                    if !text.is_empty() {
                        let x = crate::svg::number_attribute(attributes, "x").unwrap_or(0.0);
                        let y = crate::svg::number_attribute(attributes, "y").unwrap_or(0.0);
                        let shaped = self
                            .text
                            .shape_styled(
                                &text,
                                style.font_size,
                                style.direction == Direction::Rtl,
                                &style.font,
                            )
                            .map_err(|_| LayoutError::Text)?;
                        if !shaped.glyphs.is_empty() {
                            let color =
                                svg_style_color(&style.svg_fill, style.color, inherited_opacity);
                            if color.a > 0 {
                                self.push_command(Command::PushTransform(node_transform))?;
                                self.push_command(Command::GlyphRun {
                                    origin_x: x,
                                    baseline_y: y,
                                    size: style.font_size,
                                    color,
                                    glyphs: shaped.glyphs,
                                })?;
                                self.push_command(Command::PopTransform)?;
                            }
                        }
                    }
                    if opacity_layer {
                        self.push_command(Command::PopLayer)?;
                    }
                }
                continue;
            }
            self.paint_svg_shape(
                tag,
                attributes,
                &style,
                node_transform,
                viewport,
                inherited_opacity,
                user_box,
                &node_clips,
            )?;
        }
        Ok(())
    }

    fn paint_svg_use(
        &mut self,
        use_node: NodeId,
        use_style: &Style,
        attributes: &[(crate::Name, String)],
        transform: Affine,
        viewport: Rect,
        inherited_opacity: f32,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
        clips: &[Arc<crate::paint::SvgClip>],
    ) -> Result<(), LayoutError> {
        if depth > 32 || use_chain.len() >= 32 || !use_style.visibility_visible {
            return Ok(());
        }
        let Some(reference) = self.svg_href(use_node) else {
            return Ok(());
        };
        let Some(id) = reference.strip_prefix('#') else {
            return Ok(());
        };
        let Some(target) = self.find_svg_id(id)? else {
            return Ok(());
        };
        if use_chain.contains(&target) {
            return Ok(());
        }
        let coordinate = |index: usize, key: &str, basis: f32| {
            use_style.svg_geometry[index]
                .as_deref()
                .and_then(|value| crate::svg::coordinate_value(value, basis))
                .or_else(|| crate::svg::coordinate_attribute(attributes, key, basis))
                .unwrap_or(0.0)
        };
        let x = coordinate(0, "x", user_box.width);
        let y = coordinate(1, "y", user_box.height);
        let instance_transform = transform.then(Affine {
            e: x,
            f: y,
            ..Affine::IDENTITY
        });
        if use_style.opacity <= 0.0 {
            return Ok(());
        }
        let opacity_layer = use_style.opacity < 1.0;
        if opacity_layer {
            self.push_command(Command::PushLayer {
                corners: None,
                rect: viewport,
                radius: 0.0,
                opacity: use_style.opacity.clamp(0.0, 1.0),
                clip: false,
            })?;
        }
        use_chain.push(target);
        let result = self.paint_svg_instance_root(
            target,
            use_style,
            instance_transform,
            viewport,
            inherited_opacity,
            user_box,
            depth + 1,
            use_chain,
            clips,
        );
        use_chain.pop();
        if opacity_layer {
            self.push_command(Command::PopLayer)?;
        }
        result
    }

    fn paint_svg_instance_root(
        &mut self,
        target: NodeId,
        parent_style: &Style,
        transform: Affine,
        viewport: Rect,
        inherited_opacity: f32,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
        inherited_clips: &[Arc<crate::paint::SvgClip>],
    ) -> Result<(), LayoutError> {
        if depth > 32 {
            return Ok(());
        }
        let NodeKind::Element {
            namespace: Namespace::Svg,
            name,
            attributes,
        } = self
            .document
            .kind(target)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Ok(());
        };
        let tag = crate::svg::local_name(name);
        let style = self
            .computed_style(target, Some(parent_style))
            .map_err(LayoutError::Css)?;
        if style.display == Display::None || !style.visibility_visible {
            return Ok(());
        }
        if matches!(
            tag,
            "defs"
                | "symbol"
                | "svg"
                | "clipPath"
                | "mask"
                | "filter"
                | "marker"
                | "pattern"
                | "linearGradient"
                | "radialGradient"
                | "stop"
                | "title"
                | "desc"
                | "metadata"
                | "style"
                | "script"
                | "foreignObject"
        ) {
            return Ok(());
        }
        let local_transform = crate::svg::attribute(attributes, "transform")
            .and_then(crate::svg::parse_transform)
            .unwrap_or(Affine::IDENTITY);
        let target_transform = transform.then(local_transform);
        let mut clips = inherited_clips.to_vec();
        if let Some(reference) = style.svg_clip_path.as_deref() {
            if let Some(clip) = self.svg_clip(reference, user_box)? {
                clips.push(clip);
            }
        }
        if tag == "use" {
            return self.paint_svg_use(
                target,
                &style,
                attributes,
                target_transform,
                viewport,
                inherited_opacity,
                user_box,
                depth + 1,
                use_chain,
                &clips,
            );
        }
        if tag == "g" || tag == "a" {
            if style.opacity <= 0.0 {
                return Ok(());
            }
            let opacity_layer = style.opacity < 1.0;
            if opacity_layer {
                self.push_command(Command::PushLayer {
                    corners: None,
                    rect: viewport,
                    radius: 0.0,
                    opacity: style.opacity.clamp(0.0, 1.0),
                    clip: false,
                })?;
            }
            let result = self.paint_svg_children(
                target,
                &style,
                target_transform,
                viewport,
                inherited_opacity,
                user_box,
                depth + 1,
                use_chain,
                &clips,
            );
            if opacity_layer {
                self.push_command(Command::PopLayer)?;
            }
            return result;
        }
        if matches!(
            tag,
            "rect" | "circle" | "ellipse" | "line" | "path" | "polygon" | "polyline"
        ) {
            return self.paint_svg_shape(
                tag,
                attributes,
                &style,
                target_transform,
                viewport,
                inherited_opacity,
                user_box,
                &clips,
            );
        }
        Ok(())
    }

    fn collect_svg_text(
        &self,
        parent: NodeId,
        output: &mut String,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if depth > 512 || output.len() > 64 * 1024 {
            return Err(LayoutError::Text);
        }
        let mut child = self
            .document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            match self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?
            {
                NodeKind::Text(value) => {
                    if output.len().saturating_add(value.len()) > 64 * 1024 {
                        return Err(LayoutError::Text);
                    }
                    output.push_str(value);
                }
                NodeKind::Element {
                    namespace: Namespace::Svg,
                    name,
                    ..
                } if crate::svg::local_name(name) == "tspan" => {
                    self.collect_svg_text(node, output, depth + 1)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn next_paint_order(&mut self) -> u64 {
        let order = self.next_paint_order;
        self.next_paint_order = self.next_paint_order.saturating_add(1);
        order
    }

    fn discard_positioned_flow_in(&mut self, commands: Range<usize>, hits: Range<usize>) {
        self.positioned_flow_paints.retain(|paint| {
            !(paint.commands.start >= commands.start
                && paint.commands.end <= commands.end
                && paint.hits.start >= hits.start
                && paint.hits.end <= hits.end)
        });
    }
}

/// One in-flow block child recorded during the block walk for margin
/// collapse-through analysis (CSS 2.1 §8.3.1).
struct BlockChild {
    first_cmd: usize,
    first_hit: usize,
    border_top: f32,
    margin_top: f32,
    margin_bottom: f32,
    /// `Some(chain)` when the child collapsed through completely; `chain` is
    /// its single collapsed margin `max(top, bottom)`.
    through: Option<f32>,
    advance: f32,
    break_before: bool,
}

struct FlexItem {
    node: NodeId,
    generated: Option<VirtualGeneratedChild>,
    style: Style,
    main: f32,
    min_main: f32,
    source_order: usize,
    cross: f32,
    edges: f32,
    cross_edges: f32,
    frozen: bool,
    first_command: usize,
    last_command: usize,
    first_hit: usize,
    last_hit: usize,
}

/// Position in the command and hit lists where the current line began.
#[derive(Clone, Copy)]
struct LineStart {
    command: usize,
    hit: usize,
}

struct InlineTextSpan {
    range: Range<usize>,
    style_node: NodeId,
    style: Style,
    frames: Vec<usize>,
}

struct InlineFrame {
    node: NodeId,
    style: Style,
    virtual_pseudo: Option<css::PseudoElement>,
    hit: Option<usize>,
    bounds: Option<Rect>,
}

/// A generated pseudo child keeps its originating element and computed style
/// without allocating or inserting a DOM node.
#[derive(Clone)]
struct VirtualGeneratedChild {
    origin: NodeId,
    pseudo: css::PseudoElement,
    style: Style,
    unresolved_style: Style,
    items: Arc<[css::GeneratedContentItem]>,
    text: Arc<str>,
}

struct InlineAtom {
    range: Range<usize>,
    node: NodeId,
    parent_style: Style,
    style: Style,
    frames: Vec<usize>,
    width: f32,
    height: f32,
}

#[derive(Default)]
struct InlineParagraph {
    text: String,
    spans: Vec<InlineTextSpan>,
    frames: Vec<InlineFrame>,
    atoms: Vec<InlineAtom>,
    hard_breaks: Vec<usize>,
    collapse_space: bool,
    has_preceding_content: bool,
    control_node: Option<NodeId>,
    control_hit_index: Option<usize>,
    control_placeholder: bool,
    control_password: bool,
    control_source_ranges: Option<Vec<Range<usize>>>,
}

struct InlineToken {
    style: Option<Style>,
    style_node: Option<NodeId>,
    frames: Vec<usize>,
    atom: Option<usize>,
    shaped: Option<crate::paint::ShapedRun>,
    source_range: Option<Range<usize>>,
    rtl: bool,
    width: f32,
    x: f32,
    command: Option<usize>,
    hit: Option<usize>,
}

struct FormText {
    value: String,
    placeholder: bool,
    password: bool,
    multiline: bool,
}

enum InlinePart {
    Text { span: usize, range: Range<usize> },
    Atom(usize),
}

#[derive(Clone, PartialEq)]
struct TextDecoration {
    lines: u8,
    color: Rgba,
    size: f32,
    font: FontSpec,
}

#[derive(Clone, Copy)]
pub(crate) struct ScrollOffset {
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
}

#[derive(Default)]
pub(crate) struct LayoutGeometry {
    pub transforms: Vec<HitTransform>,
    pub hits: Vec<HitRegion>,
    pub rounded_clips: Vec<HitClip>,
    pub scroll_extents: Vec<ScrollOffset>,
    pub scroll_regions: Vec<ScrollRegion>,
    pub control_text_runs: Vec<ControlTextRun>,
}

/// Visual geometry for one resolved control-text run. `range` uses byte
/// offsets in the original value; `display_range` uses the shaped text.
#[derive(Clone, Debug)]
pub struct ControlTextRun {
    pub node: NodeId,
    /// UTF-8 byte range in the original control value. Password ranges cover
    /// source grapheme clusters even though their display text is masked.
    pub range: Range<usize>,
    /// UTF-8 byte range in the text that was actually shaped for this run.
    /// This differs from `range` for password values and placeholders.
    pub display_range: Range<usize>,
    pub(crate) hit_index: Option<usize>,
    pub(crate) command_index: Option<usize>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub baseline_y: f32,
    pub rtl: bool,
    pub font_size: f32,
    pub font: FontSpec,
    pub placeholder: bool,
    pub password: bool,
    /// Composed CSS transform from the run's retained layout coordinates to
    /// viewport coordinates. `x` and `y` already include scroll offsets.
    pub transform: crate::paint::Affine,
}

/// Index ranges of one scroll container's scrolled content, recorded when the
/// content can be moved by translation alone.
#[derive(Clone)]
pub(crate) struct ScrollRegion {
    pub node: NodeId,
    pub commands: core::ops::Range<usize>,
    pub hits: core::ops::Range<usize>,
    pub transforms: core::ops::Range<usize>,
    pub clips: core::ops::Range<usize>,
    /// Scroll offset the content was laid out at.
    pub base: (f32, f32),
    /// Largest distance from `base` whose content was laid out.
    pub window: (f32, f32),
}

#[derive(Clone)]
pub(crate) struct HitClip {
    pub corners: Option<Arc<[[f32; 2]; 4]>>,
    pub first_transform: usize,
    pub hits: core::ops::Range<usize>,
    pub rect: Rect,
    pub radius: f32,
}

#[derive(Clone)]
pub(crate) struct HitTransform {
    pub hits: core::ops::Range<usize>,
    pub matrix: crate::paint::Affine,
}

impl LayoutGeometry {
    pub(crate) fn contains_hit(&self, index: usize, x: f32, y: f32) -> bool {
        if self.transforms.is_empty()
            && !self
                .hits
                .get(index)
                .is_some_and(|hit| hit.rect.contains_rounded(x, y, 0.0))
        {
            return false;
        }
        let (mut local_x, mut local_y) = (x, y);
        for transform in self
            .transforms
            .iter()
            .rev()
            .filter(|v| v.hits.contains(&index))
        {
            let Some(inverse) = transform.matrix.inverse() else {
                return false;
            };
            (local_x, local_y) = inverse.apply(local_x, local_y);
        }
        let Some(hit) = self.hits.get(index) else {
            return false;
        };
        if !hit.rect.contains_rounded(local_x, local_y, 0.0) {
            return false;
        }
        self.rounded_clips
            .iter()
            .filter(|clip| clip.hits.contains(&index))
            .all(|clip| {
                let (mut clip_x, mut clip_y) = (x, y);
                for transform in self.transforms[clip.first_transform..]
                    .iter()
                    .rev()
                    .filter(|v| v.hits.start <= clip.hits.start && clip.hits.end <= v.hits.end)
                {
                    let Some(inverse) = transform.matrix.inverse() else {
                        return false;
                    };
                    (clip_x, clip_y) = inverse.apply(clip_x, clip_y);
                }
                clip.corners.as_ref().map_or_else(
                    || clip.rect.contains_rounded(clip_x, clip_y, clip.radius),
                    |r| clip.rect.contains_corners(clip_x, clip_y, r),
                )
            })
    }
    /// Moves a scroll container's laid-out content from scroll offset `from`
    /// to `to`. Returns false when `to` lies outside the laid-out window or
    /// the container has no replayable region; the caller then relays out.
    pub(crate) fn scroll_region(
        &mut self,
        list: &mut [Command],
        node: NodeId,
        from: (f32, f32),
        to: (f32, f32),
    ) -> bool {
        let Some(region) = self.scroll_regions.iter().find(|r| r.node == node) else {
            return false;
        };
        if (to.0 - region.base.0).abs() > region.window.0
            || (to.1 - region.base.1).abs() > region.window.1
            || region.commands.end > list.len()
            || region.hits.end > self.hits.len()
        {
            return false;
        }
        let (commands, hits, transforms, clips) = (
            region.commands.clone(),
            region.hits.clone(),
            region.transforms.clone(),
            region.clips.clone(),
        );
        // A text-clipped background outside this region may contain copies of
        // its glyphs. Translating only the region would leave that ink mask at
        // its old position. Ask the session to rebuild the frame instead.
        if list.iter().enumerate().any(|(index, command)| {
            !commands.contains(&index)
                && matches!(command, Command::MaskedBackground(mask) if mask.text_clipped)
        }) {
            return false;
        }
        let (dx, dy) = (from.0 - to.0, from.1 - to.1);
        for command in &mut list[commands] {
            if !matches!(command, Command::PushLayer { clip: false, .. }) {
                move_command(command, dx, dy);
            }
        }
        for hit in &mut self.hits[hits.clone()] {
            hit.rect.x += dx;
            hit.rect.y += dy;
        }
        for transform in &mut self.transforms[transforms] {
            transform.matrix = transform.matrix.translated_space(dx, dy);
        }
        for clip in &mut self.rounded_clips[clips] {
            clip.rect.x += dx;
            clip.rect.y += dy;
        }
        for run in &mut self.control_text_runs {
            if run.hit_index.is_some_and(|index| hits.contains(&index)) {
                run.x += dx;
                run.y += dy;
                run.baseline_y += dy;
            }
        }
        self.refresh_control_run_transforms(hits);
        true
    }

    fn move_hits(&mut self, range: core::ops::Range<usize>, x: f32, y: f32) {
        for transform in &mut self.transforms {
            if range.start <= transform.hits.start && transform.hits.end <= range.end {
                transform.matrix = transform.matrix.translated_space(x, y);
            }
        }
        for hit in &mut self.hits[range.clone()] {
            hit.rect.x += x;
            hit.rect.y += y;
        }
        for clip in &mut self.rounded_clips {
            if range.start <= clip.hits.start && clip.hits.end <= range.end {
                clip.rect.x += x;
                clip.rect.y += y;
            }
        }
        for run in &mut self.control_text_runs {
            if run.hit_index.is_some_and(|index| range.contains(&index)) {
                run.x += x;
                run.y += y;
                run.baseline_y += y;
            }
        }
        self.refresh_control_run_transforms(range);
    }

    fn refresh_control_run_transforms(&mut self, hits: Range<usize>) {
        let transforms = &self.transforms;
        for run in &mut self.control_text_runs {
            let Some(index) = run.hit_index.filter(|index| hits.contains(index)) else {
                continue;
            };
            run.transform = transforms
                .iter()
                .filter(|transform| transform.hits.contains(&index))
                .fold(crate::paint::Affine::IDENTITY, |matrix, transform| {
                    matrix.then(transform.matrix)
                });
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HitRegion {
    pub node: NodeId,
    pub rect: Rect,
    pub virtual_generated: bool,
}

/// Resolve percentages on the original border box, then subtract each
/// adjacent physical inset independently (CSS Backgrounds §4.3).
fn clipped_corners(rect: Rect, clip: Rect, style: &Style) -> Option<Arc<[[f32; 2]; 4]>> {
    style.border_corner_radii.as_ref()?;
    let mut r = style.corner_radii(rect.width, rect.height);
    let left = (clip.x - rect.x).max(0.0);
    let top = (clip.y - rect.y).max(0.0);
    let right = (rect.x + rect.width - clip.x - clip.width).max(0.0);
    let bottom = (rect.y + rect.height - clip.y - clip.height).max(0.0);
    for (corner, dx, dy) in [
        (0, left, top),
        (1, right, top),
        (2, right, bottom),
        (3, left, bottom),
    ] {
        r[corner][0] = (r[corner][0] - dx).max(0.0);
        r[corner][1] = (r[corner][1] - dy).max(0.0);
    }
    Some(Arc::new(r))
}

fn masked_background(paint: Command, rect: Rect, kind: BackgroundBox, style: &Style) -> Command {
    if !matches!(
        kind,
        BackgroundBox::Text | BackgroundBox::BorderArea | BackgroundBox::BorderAreaText
    ) {
        return paint;
    }
    let mut commands = Vec::new();
    if matches!(
        kind,
        BackgroundBox::BorderArea | BackgroundBox::BorderAreaText
    ) {
        commands.push(Command::StrokeBoxBorder(Box::new(
            crate::paint::BoxBorder {
                rect,
                radius: style.border_radius,
                widths: border_widths(style),
                colors: [Rgba {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                }; 4],
                pattern: style.border_pattern,
                side_patterns: Some(style.border_styles().map(|s| s.pattern())),
                corners: clipped_corners(rect, rect, style).map(|r| *r),
            },
        )));
    }
    Command::MaskedBackground(Box::new(crate::paint::MaskedBackground {
        text_clipped: matches!(kind, BackgroundBox::Text | BackgroundBox::BorderAreaText),
        rect,
        paint: Box::new(paint),
        mask: Arc::new(DisplayList(commands)),
        offset: [0.0; 2],
    }))
}

// Flex/grid stretching changes the border box after its background was
// recorded. Re-resolve the mask's percentage radii and border bands as well
// as the paint, retaining the already-shaped text at its current position.
fn stretch_background_mask(
    command: &mut Command,
    style: &Style,
    height: f32,
    layer: Option<(usize, BackgroundLayer)>,
    images: Option<&dyn ImageResolver>,
    viewport: Rect,
    bytes: &mut usize,
) -> Result<(), LayoutError> {
    let Command::MaskedBackground(old) = command else {
        return Ok(());
    };
    let mut rect = old.rect;
    rect.height = height;
    let mut replacement = if let Some((index, layer)) = layer {
        let paint = background_paint(&layer, images);
        background_command(rect, style, &layer, index, &paint, viewport, None)
    } else {
        background_color(rect, style)
    };
    if let Command::MaskedBackground(new) = &mut replacement {
        if old.text_clipped {
            let mut mask = new.mask.0.clone();
            for source in &old.mask.0 {
                if matches!(source, Command::StrokeBoxBorder(_)) {
                    continue;
                }
                let mut ink = source.clone();
                move_command(&mut ink, old.offset[0], old.offset[1]);
                mask.push(ink);
            }
            new.mask = Arc::new(DisplayList(mask));
        }
    }
    replace_command_bytes(command, bytes, replacement)
}

fn stretch_shadow(
    rect: &mut Rect,
    corners: &mut Option<Arc<[[f32; 2]; 4]>>,
    inset: bool,
    height: f32,
    style: &Style,
) {
    let [top, right, _bottom, left] = border_widths(style);
    let outer = if inset {
        Rect {
            x: rect.x - left,
            y: rect.y - top,
            width: rect.width + left + right,
            height,
        }
    } else {
        Rect { height, ..*rect }
    };
    *rect = if inset {
        padding_box(outer, style)
    } else {
        outer
    };
    *corners = clipped_corners(outer, *rect, style);
}

fn background_color(rect: Rect, style: &Style) -> Command {
    let border_box = rect;
    // The color is the bottom-most background layer, so it takes the clip
    // value at that layer's index (the per-layer lists repeat when shorter).
    let image_count = style
        .background_images
        .as_ref()
        .map_or(1, |images| images.len().min(MAX_BACKGROUND_LAYERS))
        .max(1);
    let clip_kind = cycle_layer(
        image_count - 1,
        &style.background_clip,
        BackgroundBox::Border,
    );
    let mut clip = background_box(rect, style, clip_kind);

    // `background_box` folds the opaque-border bleed optimization into its
    // border-box result. Keep the color fill centered under that border as
    // before; padding/content clips, by contrast, must stay on their edge.
    let bleed = if clip_kind == BackgroundBox::Border {
        background_bleed_inset(style)
    } else {
        0.0
    };
    if clip_kind == BackgroundBox::Border && style.border_solid {
        clip = rect;
    }
    let left = (clip.x - rect.x).max(0.0);
    let top = (clip.y - rect.y).max(0.0);
    let right = (rect.x + rect.width - clip.x - clip.width).max(0.0);
    let bottom = (rect.y + rect.height - clip.y - clip.height).max(0.0);
    // Preserve the uniform circular fast path; elliptical corners retain
    // their separate adjacent-side insets below.
    let clip_inset = left.max(top).max(right).max(bottom);
    let radius = (style.border_radius - clip_inset - bleed)
        .max(0.0)
        .min(clip.width.min(clip.height) * 0.5);
    let corners = clipped_corners(rect, inset_rect(clip, bleed), style);
    let rect = inset_rect(clip, bleed);
    let paint = if radius > 0.0 || corners.is_some() {
        Command::FillRoundedRect {
            corners,
            rect,
            radius,
            color: style.background,
        }
    } else {
        Command::FillRect {
            rect,
            color: style.background,
        }
    };
    masked_background(paint, border_box, clip_kind, style)
}

fn background_bleed_inset(style: &Style) -> f32 {
    // Blink's shrink-background path hides the fill edge behind an opaque solid border.
    if style.border_radius > 0.0
        && style.border_solid
        && style.border_pattern.is_none()
        && style.border_color.a == 255
    {
        style.border_width * 0.5
    } else {
        0.0
    }
}

fn grid_track_sizes(
    definitions: &[css::GridTrack],
    count: usize,
    available: Option<f32>,
    gap: f32,
    stretch: bool,
    contributions: impl Iterator<Item = (usize, usize, f32, f32)>,
) -> [f32; css::MAX_GRID_TRACKS] {
    use css::{GridBreadth as B, GridTrack as T, MAX_GRID_TRACKS};
    let mut sizes = [0.0f32; MAX_GRID_TRACKS];
    let mut limits = [f32::INFINITY; MAX_GRID_TRACKS];
    let fixed = |breadth| match breadth {
        B::Pixels(v) => Some(v),
        B::Percentage(v) => available.map(|size| size * v / 100.0),
        B::Length(px, pct) => available.map(|size| px + size * pct / 100.0),
        _ => None,
    };
    for i in 0..count {
        let track = definitions.get(i).copied().unwrap_or(T::Auto);
        sizes[i] = fixed(track.minimum()).unwrap_or(0.0);
        limits[i] = fixed(track.maximum())
            .unwrap_or(f32::INFINITY)
            .max(sizes[i]);
    }
    for (start, span, min, max) in contributions {
        for maximum in [false, true] {
            let eligible = |i: usize| {
                let track = definitions.get(i).copied().unwrap_or(T::Auto);
                let breadth = if maximum {
                    track.maximum()
                } else {
                    track.minimum()
                };
                if maximum {
                    matches!(breadth, B::MaxContent | B::Auto | B::MinContent)
                        && !matches!(track, T::MinMax(_, B::Fraction(_)))
                } else {
                    matches!(breadth, B::Auto | B::MinContent | B::MaxContent)
                        || matches!(breadth, B::Percentage(_)) && available.is_none()
                }
            };
            let current = sizes[start..start + span].iter().sum::<f32>() + gap * (span - 1) as f32;
            let breadth = if maximum {
                definitions.get(start).copied().unwrap_or(T::Auto).maximum()
            } else {
                definitions.get(start).copied().unwrap_or(T::Auto).minimum()
            };
            let target = if span == 1 && matches!(breadth, B::MinContent) {
                min
            } else if maximum || span == 1 && matches!(breadth, B::MaxContent) {
                max
            } else {
                min
            };
            let mut extra = (target - current).max(0.0);
            for _ in 0..span {
                let eligible_count = (start..start + span)
                    .filter(|i| eligible(*i) && sizes[*i] < limits[*i])
                    .count();
                if eligible_count == 0 || extra <= 0.0 {
                    break;
                }
                let share = extra / eligible_count as f32;
                for i in start..start + span {
                    if eligible(i) && sizes[i] < limits[i] {
                        let added = share.min(limits[i] - sizes[i]);
                        sizes[i] += added;
                        extra -= added;
                    }
                }
            }
        }
        for i in start..start + span {
            let limit = match definitions.get(i) {
                Some(T::FitContent(limit)) => Some(*limit),
                Some(T::FitContentLength(limit)) => Some(available.map_or(limit.pixels, |basis| {
                    limit.pixels + limit.percent * basis / 100.0
                })),
                _ => None,
            };
            if let Some(limit) = limit {
                sizes[i] = sizes[i].min(limit.max(min / span as f32));
            }
        }
    }
    if let Some(available) = available {
        let mut extra =
            (available - sizes[..count].iter().sum::<f32>() - gap * count.saturating_sub(1) as f32)
                .max(0.0);
        for _ in 0..count {
            let growing = (0..count)
                .filter(|i| limits[*i].is_finite() && sizes[*i] < limits[*i])
                .count();
            if growing == 0 || extra <= 0.0 {
                break;
            }
            let share = extra / growing as f32;
            for i in 0..count {
                if limits[i].is_finite() && sizes[i] < limits[i] {
                    let added = share.min(limits[i] - sizes[i]);
                    sizes[i] += added;
                    extra -= added;
                }
            }
        }
        let mut frozen = [false; MAX_GRID_TRACKS];
        for _ in 0..=count {
            let mut used = gap * count.saturating_sub(1) as f32;
            let mut fractions = 0.0;
            for i in 0..count {
                match definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    B::Fraction(v) if !frozen[i] => fractions += v,
                    _ => used += sizes[i],
                }
            }
            if fractions == 0.0 {
                break;
            }
            let unit = ((available - used).max(0.0) / fractions.max(1.0)).max(0.0);
            let mut changed = false;
            for i in 0..count {
                if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    if !frozen[i] && sizes[i] > unit * v {
                        frozen[i] = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                for i in 0..count {
                    if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum()
                    {
                        if !frozen[i] {
                            sizes[i] = unit * v;
                        }
                    }
                }
                break;
            }
        }
        if stretch {
            let extra = (available
                - sizes[..count].iter().sum::<f32>()
                - gap * count.saturating_sub(1) as f32)
                .max(0.0);
            let autos = (0..count)
                .filter(|i| {
                    matches!(
                        definitions.get(*i).copied().unwrap_or(T::Auto),
                        T::Auto | T::MinMax(_, B::Auto)
                    )
                })
                .count();
            if autos > 0 {
                for i in 0..count {
                    if matches!(
                        definitions.get(i).copied().unwrap_or(T::Auto),
                        T::Auto | T::MinMax(_, B::Auto)
                    ) {
                        sizes[i] += extra / autos as f32;
                    }
                }
            }
        }
    } else {
        let unit = (0..count)
            .filter_map(
                |i| match definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    B::Fraction(v) if v > 0.0 => Some(sizes[i] / v.max(1.0)),
                    _ => None,
                },
            )
            .fold(0.0f32, f32::max);
        for i in 0..count {
            if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                sizes[i] = sizes[i].max(unit * v);
            }
        }
    }
    sizes
}

fn has_background(style: &Style) -> bool {
    style.background.a != 0 || style.background_images.is_some() || style.shadows.is_some()
}

/// Number of commands one element's background group contains: the optional
/// color fill, one `FillBackground` per non-`none` layer, and the shadows.
fn background_paints(style: &Style) -> usize {
    usize::from(style.background.a != 0 || style.background_images.is_none())
        + background_layers(style).map_or(0, |layers| {
            layers
                .iter()
                .filter(|layer| !matches!(layer.image, BackgroundImage::None))
                .count()
        })
        + style.shadows.as_ref().map_or(0, |v| v.len())
}

fn overflow_scroll_container(style: &Style) -> bool {
    style.overflow_x.scroll_container() || style.overflow_y.scroll_container()
}

fn overflow_clips_x(style: &Style) -> bool {
    style.contain_paint || style.overflow_x.clips()
}

fn overflow_clips_y(style: &Style) -> bool {
    style.contain_paint || style.overflow_y.clips()
}

const UNCLIPPED_AXIS_EXTENT: f32 = 1.0e20;

fn overflow_clip_rect(padding_box: Rect, clip_x: bool, clip_y: bool) -> Rect {
    Rect {
        x: if clip_x {
            padding_box.x
        } else {
            -UNCLIPPED_AXIS_EXTENT
        },
        y: if clip_y {
            padding_box.y
        } else {
            -UNCLIPPED_AXIS_EXTENT
        },
        width: if clip_x {
            padding_box.width
        } else {
            2.0 * UNCLIPPED_AXIS_EXTENT
        },
        height: if clip_y {
            padding_box.height
        } else {
            2.0 * UNCLIPPED_AXIS_EXTENT
        },
    }
}

/// Cycles a per-layer property list against the layer count (CSS Backgrounds
/// §3.3: short lists repeat).
fn cycle_layer<T: Clone>(index: usize, list: &Option<Arc<[T]>>, default: T) -> T {
    match list {
        Some(list) if !list.is_empty() => list[index % list.len()].clone(),
        _ => default,
    }
}

/// Merges the per-property background values into paint-order layers.
fn background_layers(style: &Style) -> Option<Vec<BackgroundLayer>> {
    let images = style.background_images.as_ref()?;
    if images.is_empty() {
        return None;
    }
    let count = images.len().min(MAX_BACKGROUND_LAYERS);
    let mut layers = Vec::with_capacity(count);
    for index in 0..count {
        layers.push(BackgroundLayer {
            image: images[index].clone(),
            position: cycle_layer(
                index,
                &style.background_position,
                [LengthPercentage::default(); 2],
            ),
            size: cycle_layer(index, &style.background_size, BackgroundSize::AUTO),
            repeat: cycle_layer(
                index,
                &style.background_repeat,
                [BackgroundRepeat::Repeat; 2],
            ),
            clip: cycle_layer(index, &style.background_clip, BackgroundBox::Border),
            origin: cycle_layer(index, &style.background_origin, BackgroundBox::Padding),
        });
    }
    Some(layers)
}

/// Resolves the `repeat(auto-fill | auto-fit)` repetition count against the
/// definite available size (CSS Grid §7.2.3.2). A repetition is the complete
/// parsed pattern, and fixed tracks on both sides consume space before the
/// repeat count is chosen. The indefinite-size case has one repetition.
fn auto_repeat_count(
    auto: &css::GridAutoRepeat,
    available: Option<f32>,
    gap: f32,
) -> Result<usize, LayoutError> {
    use css::{GridTrack, MAX_GRID_TRACKS, definite_breadth};
    let pattern_tracks = auto.tracks.len();
    if pattern_tracks == 0 {
        return Err(LayoutError::GridLimit);
    }
    let outside_tracks = auto
        .prefix_tracks
        .len()
        .checked_add(auto.suffix_tracks.len())
        .ok_or(LayoutError::GridLimit)?;
    let capacity = MAX_GRID_TRACKS.saturating_sub(outside_tracks) / pattern_tracks;
    let maximum_repetitions = capacity;
    let gap = if gap.is_finite() {
        gap.max(0.0)
    } else {
        return Err(LayoutError::GridLimit);
    };
    let Some(available) = available else {
        return (maximum_repetitions >= 1)
            .then_some(1)
            .ok_or(LayoutError::GridLimit);
    };
    let extent = |track: GridTrack| {
        definite_breadth(track.maximum(), available)
            .or_else(|| definite_breadth(track.minimum(), available))
            .unwrap_or(0.0)
            .max(0.0)
    };
    let outside_size = auto
        .prefix_tracks
        .iter()
        .chain(auto.suffix_tracks.iter())
        .copied()
        .map(extent)
        .sum::<f32>();
    // CSS Grid §7.2.3.2 requires a UA floor for repeat-count calculations to
    // avoid division by zero (1px is the suggested value). Apply it to each
    // track in the repeated pattern, while keeping fixed prefix/suffix tracks
    // at their actual sizes.
    let pattern_size = auto
        .tracks
        .iter()
        .copied()
        .map(|track| extent(track).max(1.0))
        .sum::<f32>();
    // The base gap term is `outside + repeat_count * pattern - 1`; when
    // there are no outside tracks, the first repeated track contributes no
    // leading gutter, hence the -1 remains here.
    let outside_gap = gap * (outside_tracks as f32 - 1.0);
    let per_repeat = pattern_size + gap * pattern_tracks as f32;
    if !outside_size.is_finite() || !outside_gap.is_finite() || !per_repeat.is_finite() {
        return Err(LayoutError::GridLimit);
    }
    let room = (available - outside_size - outside_gap).max(0.0);
    if !room.is_finite() {
        return Err(LayoutError::GridLimit);
    }
    let repetitions = (room / per_repeat).floor().max(1.0);
    if !repetitions.is_finite() || repetitions > maximum_repetitions as f32 {
        return Err(LayoutError::GridLimit);
    }
    Ok(repetitions as usize)
}

fn expand_auto_repeat(
    auto: &css::GridAutoRepeat,
    repetitions: usize,
) -> Result<(Vec<css::GridTrack>, Vec<css::GridNamedLine>), LayoutError> {
    use css::GridNamedLine;
    let repeated_tracks = auto
        .tracks
        .len()
        .checked_mul(repetitions)
        .ok_or(LayoutError::GridLimit)?;
    let track_count = auto
        .prefix_tracks
        .len()
        .checked_add(repeated_tracks)
        .and_then(|count| count.checked_add(auto.suffix_tracks.len()))
        .filter(|count| *count <= css::MAX_GRID_TRACKS)
        .ok_or(LayoutError::GridLimit)?;
    let mut tracks = Vec::with_capacity(track_count);
    tracks.extend_from_slice(&auto.prefix_tracks);
    let mut names = auto.prefix_names.to_vec();
    for _ in 0..repetitions {
        let offset = tracks.len();
        tracks.extend_from_slice(&auto.tracks);
        names.extend(auto.repeat_names.iter().map(|line| GridNamedLine {
            name: line.name.clone(),
            line: offset + line.line,
        }));
    }
    let suffix_offset = tracks.len();
    tracks.extend_from_slice(&auto.suffix_tracks);
    names.extend(auto.suffix_names.iter().map(|line| GridNamedLine {
        name: line.name.clone(),
        line: suffix_offset + line.line,
    }));
    Ok((tracks, names))
}

fn used_border(style: &Style) -> f32 {
    if style.border_solid {
        style.border_width
    } else {
        0.0
    }
}

fn inset_rect(rect: Rect, inset: f32) -> Rect {
    Rect {
        x: rect.x + inset,
        y: rect.y + inset,
        width: (rect.width - 2.0 * inset).max(0.0),
        height: (rect.height - 2.0 * inset).max(0.0),
    }
}

fn padding_box(rect: Rect, style: &Style) -> Rect {
    inset_rect(rect, used_border(style))
}

fn content_box(rect: Rect, style: &Style) -> Rect {
    let border = used_border(style);
    let x = rect.x + border + style.padding_sides[3];
    let y = rect.y + border + style.padding_sides[0];
    let width =
        (rect.width - 2.0 * border - style.padding_sides[1] - style.padding_sides[3]).max(0.0);
    let height =
        (rect.height - 2.0 * border - style.padding_sides[0] - style.padding_sides[2]).max(0.0);
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn background_box(rect: Rect, style: &Style, kind: BackgroundBox) -> Rect {
    match kind {
        // The default border-box clip keeps the historical solid-border shrink
        // (the border covers the shrunken edge) so fixtures stay byte-identical.
        BackgroundBox::Text | BackgroundBox::BorderArea | BackgroundBox::BorderAreaText => rect,
        BackgroundBox::Border => {
            if style.border_solid {
                padding_box(rect, style)
            } else {
                rect
            }
        }
        BackgroundBox::Padding => padding_box(rect, style),
        BackgroundBox::Content => content_box(rect, style),
    }
}

/// Resolves one layer's drawn tile rect against its positioning area, following
/// CSS Backgrounds §3.9. Percentages in `background-position` are linear in the
/// free space, so they resolve here against `area - tile`.
fn background_tile(layer: &BackgroundLayer, area: Rect, intrinsic: Option<(f32, f32)>) -> Rect {
    // An invalid (zero) intrinsic stays Some so failed images resolve to an
    // empty tile instead of falling back to the positioning area.
    let ratio = intrinsic
        .filter(|(width, height)| *width > 0.0 && *height > 0.0)
        .map(|(width, height)| width / height);
    let (width, height) = match layer.size.kind {
        BackgroundSizeKind::Cover | BackgroundSizeKind::Contain => match ratio {
            Some(ratio) => {
                let height_at_full_width = area.width / ratio;
                let cover = layer.size.kind == BackgroundSizeKind::Cover;
                let use_full_width = if cover {
                    height_at_full_width >= area.height
                } else {
                    height_at_full_width <= area.height
                };
                if use_full_width {
                    (area.width, height_at_full_width)
                } else {
                    (area.height * ratio, area.height)
                }
            }
            None => (area.width, area.height),
        },
        BackgroundSizeKind::Explicit => {
            let width = layer.size.width.map(|v| v.resolve(area.width).max(0.0));
            let height = layer.size.height.map(|v| v.resolve(area.height).max(0.0));
            match (width, height, ratio) {
                (Some(width), Some(height), _) => (width, height),
                (Some(width), None, Some(ratio)) => (width, width / ratio),
                (None, Some(height), Some(ratio)) => (height * ratio, height),
                (Some(width), None, None) => (width, area.height),
                (None, Some(height), None) => (area.width, height),
                (None, None, _) => intrinsic.unwrap_or((area.width, area.height)),
            }
        }
    };
    if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
        return Rect {
            x: area.x,
            y: area.y,
            width: 0.0,
            height: 0.0,
        };
    }
    Rect {
        x: area.x + layer.position[0].pixels + layer.position[0].fraction * (area.width - width),
        y: area.y + layer.position[1].pixels + layer.position[1].fraction * (area.height - height),
        width,
        height,
    }
}

fn round_background_tile(
    layer: &BackgroundLayer,
    area: Rect,
    intrinsic: Option<(f32, f32)>,
    mut tile: Rect,
) -> Rect {
    let round_x = layer.repeat[0] == BackgroundRepeat::Round;
    let round_y = layer.repeat[1] == BackgroundRepeat::Round;
    if !round_x && !round_y {
        return tile;
    }
    let original_ratio = intrinsic
        .filter(|(width, height)| {
            width.is_finite() && height.is_finite() && *width > 0.0 && *height > 0.0
        })
        .map(|(width, height)| width / height);
    if round_x && tile.width > 0.0 && tile.width.is_finite() {
        let count = (area.width / tile.width).round().max(1.0);
        tile.width = area.width / count;
    }
    if round_y && tile.height > 0.0 && tile.height.is_finite() {
        let count = (area.height / tile.height).round().max(1.0);
        tile.height = area.height / count;
    }
    // A single rounded axis changes the scale of the image. If the other
    // background-size dimension is auto, restore the image's intrinsic ratio.
    if layer.size.kind == BackgroundSizeKind::Explicit && round_x && !round_y {
        if layer.size.height.is_none() {
            if let Some(ratio) = original_ratio {
                tile.height = tile.width / ratio;
            }
        }
    } else if layer.size.kind == BackgroundSizeKind::Explicit && round_y && !round_x {
        if layer.size.width.is_none() {
            if let Some(ratio) = original_ratio {
                tile.width = tile.height * ratio;
            }
        }
    }
    tile.x =
        area.x + layer.position[0].pixels + layer.position[0].fraction * (area.width - tile.width);
    tile.y = area.y
        + layer.position[1].pixels
        + layer.position[1].fraction * (area.height - tile.height);
    tile
}

fn background_attachment(style: &Style, index: usize) -> css::BackgroundAttachment {
    let attachment = style
        .background_attachment
        .as_deref()
        .filter(|values| !values.is_empty())
        .map_or(css::BackgroundAttachment::Scroll, |values| {
            values[index % values.len()]
        });
    if attachment == css::BackgroundAttachment::Local && !overflow_scroll_container(style) {
        css::BackgroundAttachment::Scroll
    } else {
        attachment
    }
}

fn has_layer_attachment(style: &Style, attachment: css::BackgroundAttachment) -> bool {
    background_layers(style).is_some_and(|layers| {
        layers.iter().enumerate().any(|(index, layer)| {
            !matches!(layer.image, BackgroundImage::None)
                && background_attachment(style, index) == attachment
        })
    })
}

fn local_background_positioning_area(
    padding_area: Rect,
    style: &Style,
    origin: BackgroundBox,
) -> Rect {
    if origin == BackgroundBox::Content {
        Rect {
            x: padding_area.x + style.padding_sides[3],
            y: padding_area.y + style.padding_sides[0],
            width: (padding_area.width - style.padding_sides[1] - style.padding_sides[3]).max(0.0),
            height: (padding_area.height - style.padding_sides[0] - style.padding_sides[2])
                .max(0.0),
        }
    } else {
        // The scrollable overflow area excludes the border. For local
        // attachment, border-box origin is therefore treated as padding-box.
        padding_area
    }
}

/// An image that failed to resolve paints nothing but still occupies its layer
/// slot so the background group command counts stay consistent.
fn empty_image() -> Arc<ImageData> {
    Arc::new(ImageData {
        width: 0,
        height: 0,
        pixels: Vec::new(),
    })
}

fn background_image_pending(image: &BackgroundImage, images: Option<&dyn ImageResolver>) -> bool {
    match image {
        BackgroundImage::Url(url) | BackgroundImage::UrlResolution { url, .. } => matches!(
            images.map(|resolver| resolver.resolve(url)),
            Some(ImageState::Pending)
        ),
        BackgroundImage::CrossFade(items) => items
            .iter()
            .any(|(image, _)| background_image_pending(image, images)),
        _ => false,
    }
}

fn background_paint(
    layer: &BackgroundLayer,
    images: Option<&dyn ImageResolver>,
) -> BackgroundPaint {
    match &layer.image {
        BackgroundImage::Solid(color) => BackgroundPaint::Solid(*color),
        BackgroundImage::CrossFade(items) => BackgroundPaint::CrossFade(
            items
                .iter()
                .map(|(image, w)| {
                    let nested = BackgroundLayer {
                        image: image.clone(),
                        ..layer.clone()
                    };
                    (background_paint(&nested, images), *w)
                })
                .collect::<Vec<_>>()
                .into(),
        ),
        BackgroundImage::None => BackgroundPaint::Image(empty_image()),
        BackgroundImage::Gradient(gradient) => BackgroundPaint::Gradient(gradient.clone()),
        BackgroundImage::Url(url) | BackgroundImage::UrlResolution { url, .. } => {
            BackgroundPaint::Image(match images.map(|images| images.resolve(url)) {
                Some(ImageState::Ready(image)) if image.is_valid() => image,
                _ => empty_image(),
            })
        }
    }
}

fn background_command(
    rect: Rect,
    style: &Style,
    layer: &BackgroundLayer,
    layer_index: usize,
    paint: &BackgroundPaint,
    viewport: Rect,
    local_padding_area: Option<Rect>,
) -> Command {
    let attachment = background_attachment(style, layer_index);
    let clip = background_box(rect, style, layer.clip);
    let radius = (style.border_radius - (clip.x - rect.x).max(clip.y - rect.y)).max(0.0);
    let positioning = match attachment {
        css::BackgroundAttachment::Fixed => viewport,
        css::BackgroundAttachment::Local => local_background_positioning_area(
            local_padding_area.unwrap_or_else(|| padding_box(rect, style)),
            style,
            layer.origin,
        ),
        css::BackgroundAttachment::Scroll => background_box(rect, style, layer.origin),
    };
    let intrinsic = match paint {
        BackgroundPaint::Image(image) if image.is_valid() => {
            let density = match &layer.image {
                BackgroundImage::UrlResolution { density, .. }
                    if density.is_finite() && *density > 0.0 =>
                {
                    *density
                }
                _ => 1.0,
            };
            Some((image.width as f32 / density, image.height as f32 / density))
        }
        // Invalid or unresolved images paint nothing but keep their layer slot.
        BackgroundPaint::Image(_) => Some((0.0, 0.0)),
        BackgroundPaint::Gradient(_) | BackgroundPaint::Solid(_) => None,
        BackgroundPaint::CrossFade(_) => paint.natural_size(),
    };
    let tile = round_background_tile(
        layer,
        positioning,
        intrinsic,
        background_tile(layer, positioning, intrinsic),
    );
    let paint_command = Command::FillBackground(Box::new(crate::paint::BackgroundFill {
        corners: clipped_corners(rect, clip, style),
        rect: clip,
        radius,
        positioning_rect: positioning,
        image_rect: tile,
        repeat: layer.repeat,
        image: paint.clone(),
    }));
    masked_background(paint_command, rect, layer.clip, style)
}

fn border(rect: Rect, style: &Style) -> Command {
    if style.border_corner_radii.is_some() {
        return side_border_commands(rect, style).remove(0);
    }
    if let Some(pattern) = style.border_pattern {
        return Command::StrokePatternBorder {
            rect,
            radius: style.border_radius,
            width: style.border_width,
            color: style.border_color,
            pattern,
        };
    }
    Command::StrokeBorder {
        rect,
        radius: style.border_radius,
        width: style.border_width,
        color: style.border_color,
    }
}

fn svg_color_opacity(mut color: Rgba, opacity: f32) -> Rgba {
    color.a = (f32::from(color.a) * opacity.clamp(0.0, 1.0))
        .round()
        .clamp(0.0, 255.0) as u8;
    color
}

fn svg_style_color(paint: &css::SvgPaint, current_color: Rgba, opacity: f32) -> Rgba {
    let color = match paint {
        css::SvgPaint::Color(color) => *color,
        css::SvgPaint::CurrentColor => current_color,
        css::SvgPaint::Unsupported(Some(color)) | css::SvgPaint::Reference(_, Some(color)) => {
            *color
        }
        css::SvgPaint::Unsupported(None) => Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        },
        css::SvgPaint::Reference(_, None) | css::SvgPaint::None => Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        },
    };
    svg_color_opacity(color, opacity)
}

fn border_widths(style: &Style) -> [f32; 4] {
    core::array::from_fn(|side| {
        let solid = style.border_solid_sides[side].unwrap_or(style.border_solid);
        if solid {
            style.border_width_sides[side].unwrap_or(style.border_width)
        } else {
            0.0
        }
    })
}

fn border_solids(style: &Style) -> [bool; 4] {
    core::array::from_fn(|side| style.border_solid_sides[side].unwrap_or(style.border_solid))
}

fn side_border_commands(rect: Rect, style: &Style) -> Vec<Command> {
    let widths = border_widths(style);
    let colors: [Rgba; 4] =
        core::array::from_fn(|side| style.border_color_sides[side].unwrap_or(style.border_color));
    let solids: [bool; 4] = border_solids(style);
    let patterns = core::array::from_fn(|side| {
        style.border_style_sides[side]
            .unwrap_or(style.border_style)
            .pattern()
    });
    if style.border_radius > 0.0
        || style.border_corner_radii.is_some()
        || patterns.iter().any(Option::is_some)
    {
        return alloc::vec![Command::StrokeBoxBorder(Box::new(
            crate::paint::BoxBorder {
                rect,
                radius: style.border_radius,
                widths,
                colors,
                pattern: style.border_pattern,
                side_patterns: Some(patterns),
                corners: clipped_corners(rect, rect, style).map(|r| *r),
            }
        ))];
    }
    let mut commands = Vec::new();
    let edges = [
        Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: widths[0],
        },
        Rect {
            x: rect.x + rect.width - widths[1],
            y: rect.y,
            width: widths[1],
            height: rect.height,
        },
        Rect {
            x: rect.x,
            y: rect.y + rect.height - widths[2],
            width: rect.width,
            height: widths[2],
        },
        Rect {
            x: rect.x,
            y: rect.y,
            width: widths[3],
            height: rect.height,
        },
    ];
    for side in 0..4 {
        if solids[side] && widths[side] > 0.0 && colors[side].a > 0 {
            commands.push(Command::FillRect {
                rect: edges[side],
                color: colors[side],
            });
        }
    }
    commands
}

fn has_nonuniform_border(style: &Style) -> bool {
    let widths = border_widths(style);
    let colors: [Rgba; 4] =
        core::array::from_fn(|side| style.border_color_sides[side].unwrap_or(style.border_color));
    let solids: [bool; 4] = border_solids(style);
    widths.iter().any(|value| *value != widths[0])
        || colors.iter().any(|value| *value != colors[0])
        || solids.iter().any(|value| *value != solids[0])
        || style
            .border_style_sides
            .iter()
            .any(|value| value.is_some_and(|value| value != style.border_style))
}

fn content_dimension(style: &Style, specified: f32) -> f32 {
    let borders = border_widths(style);
    let inset = if style.box_sizing == BoxSizing::BorderBox {
        style.padding_sides[1] + style.padding_sides[3] + borders[1] + borders[3]
    } else {
        0.0
    };
    (specified - inset).max(0.0)
}

fn content_height_dimension(style: &Style, specified: f32) -> f32 {
    let borders = border_widths(style);
    (specified
        - if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[0] + style.padding_sides[2] + borders[0] + borders[2]
        } else {
            0.0
        })
    .max(0.0)
}

fn box_edges(style: &Style, horizontal: bool) -> f32 {
    let side = usize::from(horizontal);
    let borders = border_widths(style);
    style.padding_sides[side]
        + style.padding_sides[side + 2]
        + style.margin_sides[side]
        + style.margin_sides[side + 2]
        + borders[side]
        + borders[side + 2]
}

fn specified_height(style: &Style, content: f32) -> f32 {
    let borders = border_widths(style);
    content
        + if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[0] + style.padding_sides[2] + borders[0] + borders[2]
        } else {
            0.0
        }
}

fn specified_dimension(style: &Style, content: f32) -> f32 {
    let borders = border_widths(style);
    content
        + if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[1] + style.padding_sides[3] + borders[1] + borders[3]
        } else {
            0.0
        }
}

fn constrained_width(style: &Style, content: f32) -> f32 {
    content
        .min(
            style
                .max_width
                .map_or(f32::INFINITY, |value| content_dimension(style, value)),
        )
        .max(content_dimension(style, style.min_width))
}

fn constrained_height(style: &Style, content: f32) -> f32 {
    content
        .min(style.max_height.map_or(f32::INFINITY, |value| {
            content_height_dimension(style, value)
        }))
        .max(content_height_dimension(style, style.min_height))
}

fn resolved_gap(pixels: f32, fraction: f32, basis: Option<f32>) -> f32 {
    (pixels + basis.unwrap_or(0.0) * fraction).max(0.0)
}

/// Net index adjustment when a block `[first, last)` is removed and
/// reinserted at `at` (with `at <= first`): the block follows the insertion
/// point, ranges between the insertion point and the block open up, and
/// ranges after the block are unaffected.
fn shift_index(index: &mut usize, first: usize, last: usize, at: usize) {
    if *index >= first && *index < last {
        *index = at + (*index - first);
    } else if *index >= at && *index < first {
        *index += last - first;
    }
}

fn shift_range(range: &mut core::ops::Range<usize>, first: usize, last: usize, at: usize) {
    shift_index(&mut range.start, first, last, at);
    shift_index(&mut range.end, first, last, at);
}

/// Like `shift_range` for a recorded content range that lies wholly between
/// the insertion point and the moved block.
fn shift_region(range: &mut core::ops::Range<usize>, first: usize, last: usize, at: usize) {
    if range.start >= at && range.start < first && range.end <= first {
        let len = last - first;
        range.start += len;
        range.end += len;
    }
}

/// Remap a subtree range when `[first,last)` is raised to the end of
/// `[first,end)`. Enclosing scopes retain their bounds.
fn raise_range(range: &mut core::ops::Range<usize>, first: usize, last: usize, end: usize) {
    if range.start >= first && range.end <= last {
        let offset = end - last;
        range.start += offset;
        range.end += offset;
    } else if range.start >= last && range.end <= end {
        let offset = last - first;
        range.start -= offset;
        range.end -= offset;
    }
}

fn move_command(command: &mut Command, dx: f32, dy: f32) {
    match command {
        Command::MaskedBackground(mask) => {
            mask.rect.x += dx;
            mask.rect.y += dy;
            mask.offset[0] += dx;
            mask.offset[1] += dy;
            move_command(&mut mask.paint, dx, dy);
        }
        Command::PushTransform(matrix) => *matrix = matrix.translated_space(dx, dy),
        Command::StrokeBoxBorder(border) => {
            border.rect.x += dx;
            border.rect.y += dy;
        }
        Command::SvgPath {
            bounds, transform, ..
        } => {
            bounds.x += dx;
            bounds.y += dy;
            transform.e += dx;
            transform.f += dy;
        }
        Command::FillBackground(fill) => {
            let fill = &mut **fill;
            for rect in [
                &mut fill.rect,
                &mut fill.positioning_rect,
                &mut fill.image_rect,
            ] {
                rect.x += dx;
                rect.y += dy;
            }
        }
        Command::PushClip(rect)
        | Command::PushLayer { rect, .. }
        | Command::FillRect { rect, .. }
        | Command::FillRoundedRect { rect, .. }
        | Command::FillGradient { rect, .. }
        | Command::BoxShadow { rect, .. }
        | Command::StrokeBorder { rect, .. }
        | Command::StrokePatternBorder { rect, .. }
        | Command::Image { rect, .. } => {
            rect.x += dx;
            rect.y += dy;
        }
        Command::GlyphRun {
            origin_x,
            baseline_y,
            ..
        } => {
            *origin_x += dx;
            *baseline_y += dy;
        }
        Command::PopClip | Command::PopLayer | Command::PopTransform => {}
    }
}

fn float_edges(floats: &[(Rect, Float)], left: f32, width: f32, y: f32, height: f32) -> (f32, f32) {
    let mut start = left;
    let mut end = left + width;
    for (rect, side) in floats {
        if rect.y < y + height && rect.y + rect.height > y {
            if *side == Float::Left {
                start = start.max(rect.x + rect.width);
            } else {
                end = end.min(rect.x);
            }
        }
    }
    (start, (end - start).max(0.0))
}

impl Layout<'_> {
    fn decorate_text(
        &mut self,
        x: f32,
        baseline: f32,
        width: f32,
        style: &Style,
    ) -> Result<(), LayoutError> {
        if width <= 0.0 {
            return Ok(());
        }
        let count = self
            .decorations
            .len()
            .max(usize::from(style.text_decoration != 0));
        for index in 0..count {
            let source = self
                .decorations
                .get(index)
                .cloned()
                .unwrap_or(TextDecoration {
                    lines: style.text_decoration,
                    color: style.color,
                    size: style.font_size,
                    font: style.font.clone(),
                });
            let (underline, thickness) = self
                .text
                .underline_metrics_styled(source.size, &source.font);
            let (strike, strike_thickness) =
                self.text.strike_metrics_styled(source.size, &source.font);
            for (bit, offset, thickness) in [
                (1, underline, thickness),
                (
                    2,
                    -self.text.ascent_styled(source.size, &source.font),
                    thickness,
                ),
                (4, strike, strike_thickness),
            ] {
                if source.lines & bit != 0 {
                    self.push_command(Command::FillRect {
                        rect: Rect {
                            x,
                            y: baseline + offset,
                            width,
                            height: thickness.max(1.0),
                        },
                        color: source.color,
                    })?;
                }
            }
        }
        Ok(())
    }

    fn begin_decoration(&mut self, style: &Style) -> Result<(), LayoutError> {
        if style.text_decoration != 0 {
            if self.decorations.len() >= 512 {
                return Err(LayoutError::DepthLimit);
            }
            self.decorations
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            self.decorations.push(TextDecoration {
                lines: style.text_decoration,
                color: style.color,
                size: style.font_size,
                font: style.font.clone(),
            });
        }
        Ok(())
    }
    fn push_background(&mut self, rect: Rect, style: &Style) -> Result<(), LayoutError> {
        if !style.visibility_visible {
            return Ok(());
        }
        if let Some(shadows) = &style.shadows {
            for shadow in shadows.iter().rev().filter(|shadow| !shadow.inset) {
                self.push_command(Command::BoxShadow {
                    corners: clipped_corners(rect, rect, style),
                    rect,
                    radius: style.border_radius,
                    shadow: *shadow,
                })?;
            }
        }
        if style.background.a != 0 || style.background_images.is_none() {
            self.push_command(background_color(rect, style))?;
        }
        if let Some(layers) = background_layers(style) {
            for (layer_index, layer) in layers.iter().enumerate().rev() {
                if matches!(layer.image, BackgroundImage::None) {
                    continue;
                }
                if background_image_pending(&layer.image, self.images) {
                    return Err(LayoutError::ImagePending);
                }
                let paint = background_paint(&layer, self.images);
                self.push_command(background_command(
                    rect,
                    style,
                    layer,
                    layer_index,
                    &paint,
                    self.viewport,
                    None,
                ))?;
            }
        }
        if let Some(shadows) = &style.shadows {
            let border = used_border(style);
            let corners = clipped_corners(rect, inset_rect(rect, border), style);
            let rect = inset_rect(rect, border);
            for shadow in shadows.iter().rev().filter(|shadow| shadow.inset) {
                self.push_command(Command::BoxShadow {
                    corners: corners.clone(),
                    rect,
                    radius: (style.border_radius - border).max(0.0),
                    shadow: *shadow,
                })?;
            }
        }
        Ok(())
    }

    fn finish_background(
        &mut self,
        index: usize,
        rect: Rect,
        style: &Style,
        local_padding_area: Option<Rect>,
    ) -> Result<(), LayoutError> {
        let padding_rect = padding_box(rect, style);
        let first = index + 1 - background_paints(style);
        let mut layers = background_layers(style)
            .map(|layers| {
                layers
                    .into_iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, layer)| !matches!(layer.image, BackgroundImage::None))
                    .collect::<Vec<_>>()
                    .into_iter()
            })
            .unwrap_or_default();
        let needs_ink = style.background_clip.as_ref().is_some_and(|clips| {
            clips
                .iter()
                .any(|kind| matches!(kind, BackgroundBox::Text | BackgroundBox::BorderAreaText))
        });
        let mut ink = Vec::new();
        if needs_ink {
            for command in &self.commands[index + 1..] {
                match command {
                    Command::GlyphRun { .. } => {
                        let mut run = command.clone();
                        if let Command::GlyphRun { color, .. } = &mut run {
                            *color = Rgba {
                                r: 255,
                                g: 255,
                                b: 255,
                                a: 255,
                            };
                        }
                        ink.push(run);
                    }
                    Command::PushTransform(_)
                    | Command::PopTransform
                    | Command::PushClip(_)
                    | Command::PopClip
                    | Command::PopLayer => ink.push(command.clone()),
                    Command::PushLayer { .. } => {
                        let mut layer = command.clone();
                        if let Command::PushLayer { opacity, .. } = &mut layer {
                            *opacity = 1.0;
                        }
                        ink.push(layer);
                    }
                    _ => {}
                }
            }
        }
        let mut command_bytes = self.command_bytes;
        for command in &mut self.commands[first..=index] {
            if let Command::MaskedBackground(mask) = command {
                let paint = (*mask.paint).clone();
                replace_command_bytes(command, &mut command_bytes, paint)?;
            }
            match command {
                Command::FillBackground(_) => {
                    if let Some((layer_index, layer)) = layers.next() {
                        let paint = background_paint(&layer, self.images);
                        replace_command_bytes(
                            command,
                            &mut command_bytes,
                            background_command(
                                rect,
                                style,
                                &layer,
                                layer_index,
                                &paint,
                                self.viewport,
                                local_padding_area,
                            ),
                        )?;
                    }
                }
                Command::BoxShadow {
                    rect: target,
                    corners,
                    shadow,
                    ..
                } => {
                    *target = if shadow.inset { padding_rect } else { rect };
                    *corners = clipped_corners(rect, *target, style);
                }
                Command::FillRect { .. } | Command::FillRoundedRect { .. } => {
                    replace_command_bytes(
                        command,
                        &mut command_bytes,
                        background_color(rect, style),
                    )?
                }
                _ => {}
            }
            if let Command::MaskedBackground(mask) = command {
                let kind = mask.text_clipped;
                if kind && !ink.is_empty() {
                    let mut replacement = (**mask).clone();
                    let mut combined = replacement.mask.0.clone();
                    combined.extend(ink.iter().cloned());
                    replacement.mask = Arc::new(DisplayList(combined));
                    replace_command_bytes(
                        command,
                        &mut command_bytes,
                        Command::MaskedBackground(Box::new(replacement)),
                    )?;
                }
            }
        }
        self.command_bytes = command_bytes;
        Ok(())
    }
    fn table_rows(
        &self,
        id: NodeId,
        style: &Style,
        rows: &mut Vec<(NodeId, Style)>,
        groups: &mut Vec<TableRowGroup>,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let mut children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            if matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
                let computed = self
                    .computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?;
                if computed.display == Display::TableRow {
                    if rows.len() == 4096 {
                        return Err(LayoutError::CommandLimit);
                    }
                    rows.push((node, computed));
                } else if computed.display == Display::TableRowGroup {
                    let first_row = rows.len();
                    self.table_rows(node, &computed, rows, groups, depth + 1)?;
                    let end_row = rows.len();
                    if end_row > first_row {
                        if groups.len() == 4096 {
                            return Err(LayoutError::CommandLimit);
                        }
                        groups
                            .try_reserve(1)
                            .map_err(|_| LayoutError::CommandLimit)?;
                        groups.push(TableRowGroup {
                            first_row,
                            end_row,
                            style: computed,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn first_table_cell_baseline_style(
        &self,
        id: NodeId,
        style: &Style,
        depth: usize,
    ) -> Result<Option<(Style, f32)>, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let mut children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            match self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?
            {
                NodeKind::Text(text) => {
                    let has_line_content = if matches!(
                        style.white_space,
                        css::WhiteSpace::NoWrap
                            | css::WhiteSpace::Pre
                            | css::WhiteSpace::PreWrap
                            | css::WhiteSpace::BreakSpaces
                    ) {
                        !text.is_empty()
                    } else {
                        !collapsed_text(text).trim_matches(' ').is_empty()
                    };
                    if has_line_content {
                        return Ok(Some((style.clone(), 0.0)));
                    }
                }
                NodeKind::Element {
                    name,
                    namespace: Namespace::Html,
                    ..
                } => {
                    let child_style = self
                        .computed_style(node, Some(style))
                        .map_err(LayoutError::Css)?;
                    if child_style.display == Display::None
                        || matches!(child_style.position, Position::Absolute | Position::Fixed)
                        || child_style.float != Float::None
                    {
                        continue;
                    }
                    if name == "br" {
                        return Ok(Some((style.clone(), 0.0)));
                    }
                    let child_inset =
                        if matches!(child_style.display, Display::Inline | Display::InlineBlock) {
                            0.0
                        } else {
                            child_style.margin_sides[0]
                                + child_style.padding_sides[0]
                                + border_widths(&child_style)[0]
                        };
                    if let Some((baseline_style, offset)) =
                        self.first_table_cell_baseline_style(node, &child_style, depth + 1)?
                    {
                        return Ok(Some((baseline_style, child_inset + offset)));
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn table(
        &mut self,
        id: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        struct Cell {
            node: NodeId,
            style: Style,
            row: usize,
            col: usize,
            cols: usize,
            rows: usize,
            natural_width: f32,
            height: f32,
            baseline_style: Option<(Style, f32)>,
            baseline_offset: Option<f32>,
        }
        let mut rows = Vec::new();
        let mut row_groups = Vec::new();
        self.table_rows(id, style, &mut rows, &mut row_groups, depth)?;
        let mut caption = None;
        let mut direct = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = direct.next().map_err(|_| LayoutError::InvalidTree)? {
            if matches!(
                self.document.kind(node),
                Ok(NodeKind::Element { name, .. }) if name == "caption"
            ) {
                caption = Some((
                    node,
                    self.computed_style(node, Some(style))
                        .map_err(LayoutError::Css)?,
                ));
                break;
            }
        }
        let mut occupied = [0usize; 256];
        let mut widths = [0f32; 256];
        let mut cells = Vec::new();
        let mut columns = 0;
        let [spacing_x, spacing_y] = if style.border_collapse {
            [0.0, 0.0]
        } else {
            [
                style.border_spacing[0].max(0.0),
                style.border_spacing[1].max(0.0),
            ]
        };
        for (row, (node, parent)) in rows.iter().enumerate() {
            let mut col = 0;
            let mut children = self
                .document
                .composed_children_iter(*node)
                .map_err(|_| LayoutError::InvalidTree)?;
            while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
                if let Ok(NodeKind::Element { attributes, .. }) = self.document.kind(node) {
                    let computed = self
                        .computed_style(node, Some(parent))
                        .map_err(LayoutError::Css)?;
                    if computed.display == Display::TableCell {
                        let span = |key: &str, limit: usize| {
                            attributes
                                .iter()
                                .find(|(name, _)| name == key)
                                .and_then(|(_, v)| v.parse::<usize>().ok())
                                .unwrap_or(1)
                                .max(1)
                                .min(limit)
                        };
                        let cols = span("colspan", 256);
                        while col + cols <= 256
                            && occupied[col..col + cols].iter().any(|end| *end > row)
                        {
                            col += 1;
                        }
                        if col + cols > 256 || cells.len() == 4096 {
                            return Err(LayoutError::CommandLimit);
                        }
                        let rowspan = if attributes
                            .iter()
                            .any(|(name, v)| name == "rowspan" && v == "0")
                        {
                            rows.len() - row
                        } else {
                            span("rowspan", rows.len() - row)
                        };
                        for slot in &mut occupied[col..col + cols] {
                            *slot = row + rowspan;
                        }
                        let (width, height) = self.intrinsic_size(node, &computed, depth + 1)?;
                        let baseline_style =
                            self.first_table_cell_baseline_style(node, &computed, depth + 1)?;
                        let natural_width = if style.border_collapse {
                            let borders = border_widths(&computed);
                            (width - borders[1] - borders[3]).max(0.0)
                        } else {
                            width
                        };
                        if !style.border_collapse
                            && (!style.table_fixed || style.width.is_none() || row == 0)
                        {
                            let width = if style.table_fixed && style.width.is_some() {
                                computed.width.unwrap_or(0.0)
                            } else {
                                width
                            };
                            if style.table_fixed && style.width.is_some() {
                                // CSS 2.1 §17.5.2.1 divides a first-row
                                // spanning-cell width across its columns.
                                let per_column = width / cols as f32;
                                for slot in &mut widths[col..col + cols] {
                                    *slot = slot.max(per_column);
                                }
                            } else {
                                // In auto layout, the cell's intrinsic width
                                // includes the spacing between the columns it
                                // spans. That spacing is not a track width.
                                // Keep the spanning constraint on its first
                                // track; distributing it evenly can make a
                                // later empty cell consume half the table.
                                let track_width = (width - spacing_x * (cols - 1) as f32).max(0.0);
                                widths[col] = widths[col].max(track_width);
                            }
                        }
                        cells.push(Cell {
                            node,
                            style: computed,
                            row,
                            col,
                            cols,
                            rows: rowspan,
                            natural_width,
                            height,
                            baseline_style,
                            baseline_offset: None,
                        });
                        col += cols;
                        columns = columns.max(col);
                    }
                }
            }
        }
        let mut column_spans = Vec::new();
        let mut next_column = 0usize;
        let mut column_children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = column_children
            .next()
            .map_err(|_| LayoutError::InvalidTree)?
        {
            let Ok(NodeKind::Element {
                name, attributes, ..
            }) = self.document.kind(node)
            else {
                continue;
            };
            let span = attributes
                .iter()
                .find(|(name, _)| name == "span")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
                .min(256);
            if name == "col" {
                let column_style = self
                    .computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?;
                column_spans
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                column_spans.push(TableColumnSpan {
                    first_column: next_column,
                    end_column: next_column + span,
                    style: column_style,
                    origin_rank: 3,
                });
                next_column += span;
            } else if name == "colgroup" {
                let group_style = self
                    .computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?;
                let group_start = next_column;
                let mut has_columns = false;
                let mut group_children = self
                    .document
                    .composed_children_iter(node)
                    .map_err(|_| LayoutError::InvalidTree)?;
                while let Some(column) = group_children
                    .next()
                    .map_err(|_| LayoutError::InvalidTree)?
                {
                    let Ok(NodeKind::Element {
                        name, attributes, ..
                    }) = self.document.kind(column)
                    else {
                        continue;
                    };
                    if name != "col" {
                        continue;
                    }
                    has_columns = true;
                    let span = attributes
                        .iter()
                        .find(|(name, _)| name == "span")
                        .and_then(|(_, value)| value.parse::<usize>().ok())
                        .unwrap_or(1)
                        .max(1)
                        .min(256);
                    let column_style = self
                        .computed_style(column, Some(&group_style))
                        .map_err(LayoutError::Css)?;
                    column_spans
                        .try_reserve(2)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    column_spans.push(TableColumnSpan {
                        first_column: next_column,
                        end_column: next_column + span,
                        style: column_style,
                        origin_rank: 3,
                    });
                    next_column += span;
                }
                if !has_columns {
                    next_column = group_start + span;
                }
                if next_column > group_start {
                    column_spans.push(TableColumnSpan {
                        first_column: group_start,
                        end_column: next_column,
                        style: group_style,
                        origin_rank: 2,
                    });
                }
            }
        }
        columns = columns.max(next_column);
        if columns > widths.len() {
            return Err(LayoutError::CommandLimit);
        }
        let (collapsed_horizontal, collapsed_vertical) = if style.border_collapse {
            let edge_segments = (rows.len() + 1)
                .checked_mul(columns)
                .and_then(|horizontal| {
                    columns
                        .checked_add(1)
                        .and_then(|lines| lines.checked_mul(rows.len()))
                        .and_then(|vertical| horizontal.checked_add(vertical))
                })
                .ok_or(LayoutError::CommandLimit)?;
            if edge_segments > MAX_COLLAPSED_BORDER_CANDIDATES {
                return Err(LayoutError::CommandLimit);
            }
            let mut horizontal = empty_collapsed_edges(rows.len() + 1, columns)?;
            let mut vertical = empty_collapsed_edges(columns + 1, rows.len())?;
            let mut offered = 0usize;
            let rtl = style.direction == Direction::Rtl;
            for cell in &cells {
                let end_col = (cell.col + cell.cols).min(columns);
                let end_row = (cell.row + cell.rows).min(rows.len());
                offer_collapsed_border_rect(
                    &mut horizontal[cell.row][cell.col..end_col],
                    &cell.style,
                    &[0],
                    6,
                    cell.row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut horizontal[end_row][cell.col..end_col],
                    &cell.style,
                    &[2],
                    6,
                    cell.row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut vertical[cell.col][cell.row..end_row],
                    &cell.style,
                    &[3],
                    6,
                    cell.col,
                    true,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut vertical[end_col][cell.row..end_row],
                    &cell.style,
                    &[1],
                    6,
                    cell.col,
                    true,
                    rtl,
                    &mut offered,
                )?;
            }
            for (row, (_, row_style)) in rows.iter().enumerate() {
                offer_collapsed_border_rect(
                    &mut horizontal[row][..],
                    row_style,
                    &[0],
                    5,
                    row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut horizontal[row + 1][..],
                    row_style,
                    &[2],
                    5,
                    row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                if rows.len() > 0 {
                    offer_collapsed_border_rect(
                        &mut vertical[0][row..row + 1],
                        row_style,
                        &[3],
                        5,
                        row,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                    offer_collapsed_border_rect(
                        &mut vertical[columns][row..row + 1],
                        row_style,
                        &[1],
                        5,
                        row,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                }
            }
            for group in &row_groups {
                offer_collapsed_border_rect(
                    &mut horizontal[group.first_row][..],
                    &group.style,
                    &[0],
                    4,
                    group.first_row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut horizontal[group.end_row][..],
                    &group.style,
                    &[2],
                    4,
                    group.first_row,
                    false,
                    rtl,
                    &mut offered,
                )?;
                for row in group.first_row..group.end_row {
                    offer_collapsed_border_rect(
                        &mut vertical[0][row..row + 1],
                        &group.style,
                        &[3],
                        4,
                        group.first_row,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                    offer_collapsed_border_rect(
                        &mut vertical[columns][row..row + 1],
                        &group.style,
                        &[1],
                        4,
                        group.first_row,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                }
            }
            for span in &column_spans {
                let first = span.first_column.min(columns);
                let end = span.end_column.min(columns);
                for boundary in first..end {
                    offer_collapsed_border_rect(
                        &mut vertical[boundary][..],
                        &span.style,
                        &[3],
                        span.origin_rank,
                        boundary,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                }
                if end > first {
                    offer_collapsed_border_rect(
                        &mut vertical[end][..],
                        &span.style,
                        &[1],
                        span.origin_rank,
                        first,
                        true,
                        rtl,
                        &mut offered,
                    )?;
                    offer_collapsed_border_rect(
                        &mut horizontal[0][first..end],
                        &span.style,
                        &[0],
                        span.origin_rank,
                        first,
                        false,
                        rtl,
                        &mut offered,
                    )?;
                    offer_collapsed_border_rect(
                        &mut horizontal[rows.len()][first..end],
                        &span.style,
                        &[2],
                        span.origin_rank,
                        first,
                        false,
                        rtl,
                        &mut offered,
                    )?;
                }
            }
            offer_collapsed_border_rect(
                &mut horizontal[0][..],
                style,
                &[0],
                1,
                0,
                false,
                rtl,
                &mut offered,
            )?;
            offer_collapsed_border_rect(
                &mut horizontal[rows.len()][..],
                style,
                &[2],
                1,
                0,
                false,
                rtl,
                &mut offered,
            )?;
            for row in 0..rows.len() {
                offer_collapsed_border_rect(
                    &mut vertical[0][row..row + 1],
                    style,
                    &[3],
                    1,
                    0,
                    true,
                    rtl,
                    &mut offered,
                )?;
                offer_collapsed_border_rect(
                    &mut vertical[columns][row..row + 1],
                    style,
                    &[1],
                    1,
                    0,
                    true,
                    rtl,
                    &mut offered,
                )?;
            }
            (Some(horizontal), Some(vertical))
        } else {
            (None, None)
        };
        let collapsed_x_widths = collapsed_vertical.as_ref().map(|edges| {
            edges
                .iter()
                .map(|line| {
                    line.iter()
                        .filter_map(|candidate| *candidate)
                        .map(|candidate| candidate.width)
                        .fold(0.0f32, f32::max)
                })
                .collect::<Vec<_>>()
        });
        let collapsed_y_widths = collapsed_horizontal.as_ref().map(|edges| {
            edges
                .iter()
                .map(|line| {
                    line.iter()
                        .filter_map(|candidate| *candidate)
                        .map(|candidate| candidate.width)
                        .fold(0.0f32, f32::max)
                })
                .collect::<Vec<_>>()
        });
        if let Some(x_edges) = &collapsed_x_widths {
            for cell in &cells {
                if !style.table_fixed || style.width.is_none() || cell.row == 0 {
                    let end_col = (cell.col + cell.cols).min(columns);
                    let end_row = (cell.row + cell.rows).min(rows.len());
                    let mut intrinsic = cell.natural_width;
                    if style.table_fixed && style.width.is_some() && cell.row == 0 {
                        intrinsic = (cell.style.width.unwrap_or(intrinsic)
                            - border_widths(&cell.style)[1]
                            - border_widths(&cell.style)[3])
                            .max(0.0);
                    }
                    let left = collapsed_edge_width(
                        collapsed_vertical.as_deref().unwrap(),
                        cell.col,
                        cell.row,
                        end_row,
                    );
                    let right = collapsed_edge_width(
                        collapsed_vertical.as_deref().unwrap(),
                        end_col,
                        cell.row,
                        end_row,
                    );
                    let internal = x_edges[cell.col + 1..end_col].iter().sum::<f32>();
                    let required = (intrinsic + left * 0.5 + right * 0.5 - internal).max(0.0);
                    widths[cell.col] = widths[cell.col].max(required);
                }
            }
        }
        let inset = if style.border_collapse {
            0.0
        } else {
            style.padding
                + if style.border_solid {
                    style.border_width
                } else {
                    0.0
                }
        };
        let initial_natural: f32 = widths[..columns].iter().sum();
        let gaps = spacing_x * (columns + 1) as f32;
        let collapsed_edge_sum = collapsed_x_widths.as_ref().map_or(0.0, |edges| {
            (edges.first().copied().unwrap_or(0.0) + edges.last().copied().unwrap_or(0.0)) * 0.5
        });
        let initial_used = initial_natural + gaps + collapsed_edge_sum;
        let initial_width = style
            .width
            .map(|w| content_dimension(style, w))
            .unwrap_or(initial_used.min(available))
            .max(initial_used);
        if style.table_fixed && style.width.is_some() {
            // CSS 2.1 §17.5.2.1: in fixed layout, first-row cell widths
            // establish the columns. Percentage cell widths resolve against
            // the table's track area after border spacing is removed.
            let track_width = (initial_width - gaps - collapsed_edge_sum).max(0.0);
            for cell in cells.iter().filter(|cell| cell.row == 0) {
                let resolved = cell
                    .style
                    .resolve_percentages(track_width, self.parent_height);
                if let Some(cell_width) = resolved.width {
                    let cell_width = if let (Some(vertical), Some(x_edges)) =
                        (&collapsed_vertical, &collapsed_x_widths)
                    {
                        let end_col = (cell.col + cell.cols).min(columns);
                        let end_row = (cell.row + cell.rows).min(rows.len());
                        let left = collapsed_edge_width(vertical, cell.col, cell.row, end_row);
                        let right = collapsed_edge_width(vertical, end_col, cell.row, end_row);
                        let internal = x_edges[cell.col + 1..end_col].iter().sum::<f32>();
                        let padding = cell.style.padding_sides[1] + cell.style.padding_sides[3];
                        (cell_width + padding + (left + right) * 0.5 - internal).max(0.0)
                    } else {
                        cell_width
                    };
                    let per_column = cell_width / cell.cols as f32;
                    for slot in &mut widths[cell.col..cell.col + cell.cols] {
                        *slot = slot.max(per_column);
                    }
                }
            }
        }
        let natural: f32 = widths[..columns].iter().sum();
        let natural_used = natural + gaps + collapsed_edge_sum;
        let width = style
            .width
            .map(|w| content_dimension(style, w))
            .unwrap_or(natural_used.min(available))
            .max(natural_used);
        let margin_top = style.margin_sides[0];
        let margin_bottom = style.margin_sides[2];
        let margin_left = style.margin_sides[3];
        let ox = x + margin_left;
        let mut oy = y + margin_top;
        let mut caption_height = 0.0;
        if !style.caption_bottom {
            if let Some((caption_node, mut caption_style)) = caption.clone() {
                caption_style.display = Display::Block;
                caption_style.margin = 0.0;
                caption_style.box_sizing = BoxSizing::BorderBox;
                caption_style.width = Some(width);
                caption_style.height = None;
                caption_height = self.box_for(
                    caption_node,
                    style,
                    Some(caption_style),
                    ox,
                    oy,
                    width,
                    depth + 1,
                )?;
                oy += caption_height;
            }
        }
        let remaining = (width - gaps - collapsed_edge_sum - natural).max(0.0);
        let unspecified = widths[..columns].iter().filter(|w| **w == 0.0).count();
        for slot in &mut widths[..columns] {
            if style.table_fixed && style.width.is_some() && unspecified > 0 {
                if *slot == 0.0 {
                    *slot = remaining / unspecified as f32;
                }
            } else if !style.table_fixed && natural > 0.0 {
                // CSS 2.1 §17.5.2.2: when an auto-layout table has more room
                // than its max-content columns require, grow the columns in
                // proportion to those widths instead of giving every column
                // the same additive share.
                *slot += remaining * (*slot / natural);
            } else {
                *slot += remaining / columns.max(1) as f32;
            }
        }
        let mut heights = alloc::vec![0f32; rows.len()];
        let mut baseline_ascent = alloc::vec![0.0f32; rows.len()];
        let mut baseline_descent = alloc::vec![0.0f32; rows.len()];
        let required_cell_height = |cell: &Cell| {
            if let (Some(horizontal), Some(y_edges)) = (&collapsed_horizontal, &collapsed_y_widths)
            {
                let end_col = (cell.col + cell.cols).min(columns);
                let end_row = (cell.row + cell.rows).min(rows.len());
                let borders = border_widths(&cell.style);
                let top = collapsed_edge_width(horizontal, cell.row, cell.col, end_col);
                let bottom = collapsed_edge_width(horizontal, end_row, cell.col, end_col);
                (cell.height - borders[0] - borders[2] + top * 0.5 + bottom * 0.5
                    - y_edges[cell.row + 1..end_row].iter().sum::<f32>())
                .max(0.0)
            } else {
                cell.height
            }
        };
        for cell in &mut cells {
            let required = required_cell_height(cell);
            if cell.rows == 1 {
                let aligns_at_baseline = matches!(
                    cell.style.vertical_align,
                    css::VerticalAlign::Baseline
                        | css::VerticalAlign::Sub
                        | css::VerticalAlign::Super
                        | css::VerticalAlign::TextTop
                        | css::VerticalAlign::TextBottom
                        | css::VerticalAlign::Length(_)
                );
                if aligns_at_baseline {
                    if let Some((font, extra)) = &cell.baseline_style {
                        let top_border = if let Some(horizontal) = &collapsed_horizontal {
                            let end_col = (cell.col + cell.cols).min(columns);
                            collapsed_edge_width(horizontal, cell.row, cell.col, end_col) * 0.5
                        } else {
                            border_widths(&cell.style)[0]
                        };
                        let offset = (top_border
                            + cell.style.padding_sides[0]
                            + *extra
                            + self.baseline(font))
                        .clamp(0.0, required);
                        cell.baseline_offset = Some(offset);
                        baseline_ascent[cell.row] = baseline_ascent[cell.row].max(offset);
                        baseline_descent[cell.row] =
                            baseline_descent[cell.row].max((required - offset).max(0.0));
                    } else {
                        // A cell without a line box contributes its used height
                        // to the row minimum, but has no text baseline to pull
                        // the row baseline down.
                        heights[cell.row] = heights[cell.row].max(required);
                    }
                } else {
                    heights[cell.row] = heights[cell.row].max(required);
                }
            }
        }
        for row in 0..rows.len() {
            heights[row] = heights[row].max(baseline_ascent[row] + baseline_descent[row]);
            if let Some(row_height) = rows[row].1.height {
                heights[row] = heights[row].max(content_height_dimension(&rows[row].1, row_height));
            }
        }
        for cell in &cells {
            if cell.rows == 1 {
                continue;
            }
            let end_row = (cell.row + cell.rows).min(rows.len());
            let present = heights[cell.row..end_row].iter().sum::<f32>()
                + if style.border_collapse {
                    0.0
                } else {
                    spacing_y * (cell.rows - 1) as f32
                };
            let required = required_cell_height(cell);
            let extra = (required - present).max(0.0) / cell.rows as f32;
            for slot in &mut heights[cell.row..cell.row + cell.rows] {
                *slot += extra;
            }
        }
        if let Some(table_height) = style.height {
            let target = content_height_dimension(style, table_height);
            let edge_total = collapsed_y_widths.as_ref().map_or(0.0, |edges| {
                (edges.first().copied().unwrap_or(0.0) + edges.last().copied().unwrap_or(0.0)) * 0.5
            });
            let spacing_total = spacing_y * (rows.len() + 1) as f32;
            let desired_rows = (target - edge_total - spacing_total).max(0.0);
            let current_rows = heights.iter().sum::<f32>();
            let extra = (desired_rows - current_rows).max(0.0) / rows.len().max(1) as f32;
            for row_height in &mut heights {
                *row_height += extra;
            }
        }
        let collapsed_vertical_sum = collapsed_y_widths.as_ref().map_or(0.0, |edges| {
            (edges.first().copied().unwrap_or(0.0) + edges.last().copied().unwrap_or(0.0)) * 0.5
        });
        let height = heights.iter().sum::<f32>()
            + spacing_y * (rows.len() + 1) as f32
            + collapsed_vertical_sum;
        let rect = Rect {
            x: ox,
            y: oy,
            width: width + inset * 2.0,
            height: height + inset * 2.0,
        };
        let hit = self.begin_hit(id)?;
        self.finish_hit(hit, rect);
        if has_background(style) {
            self.push_background(rect, style)?;
        }
        if style.border_solid && !style.border_collapse {
            self.push_command(border(rect, style))?;
        }
        let mut xs = [0f32; 257];
        for col in 0..columns {
            // Column tracks already include their half collapsed-border
            // shares from the intrinsic cell constraints above.
            xs[col + 1] = xs[col] + widths[col] + spacing_x;
        }
        let mut ys = Vec::with_capacity(heights.len() + 1);
        ys.push(0.0);
        for height in heights {
            // Row heights likewise include their collapsed-border shares.
            ys.push(ys.last().copied().unwrap_or(0.0) + height + spacing_y);
        }
        for (row, (node, computed)) in rows.iter().enumerate() {
            let (row_x, row_y, row_width, row_height) = if style.border_collapse {
                let x_edges = collapsed_x_widths.as_deref().unwrap();
                let y_edges = collapsed_y_widths.as_deref().unwrap();
                let start_x = if style.direction == Direction::Rtl {
                    ox + width - x_edges[0] * 0.5 - xs[columns]
                } else {
                    ox + x_edges[0] * 0.5
                };
                (
                    start_x,
                    oy + y_edges[0] * 0.5 + ys[row],
                    xs[columns],
                    ys[row + 1] - ys[row],
                )
            } else {
                (
                    ox + inset + spacing_x,
                    oy + inset + spacing_y + ys[row],
                    (width - 2.0 * spacing_x).max(0.0),
                    ys[row + 1] - ys[row] - spacing_y,
                )
            };
            let rect = Rect {
                x: row_x,
                y: row_y,
                width: row_width,
                height: row_height,
            };
            let hit = self.begin_hit(*node)?;
            self.finish_hit(hit, rect);
            if has_background(computed) {
                self.push_background(rect, computed)?;
            }
        }
        for cell in cells {
            let end_col = (cell.col + cell.cols).min(columns);
            let end_row = (cell.row + cell.rows).min(rows.len());
            let cw = xs[end_col] - xs[cell.col] - spacing_x;
            let ch = ys[end_row] - ys[cell.row] - spacing_y;
            let (cx, cy) = if style.border_collapse {
                let x_edges = collapsed_x_widths.as_deref().unwrap();
                let y_edges = collapsed_y_widths.as_deref().unwrap();
                let cell_x = if style.direction == Direction::Rtl {
                    ox + width - x_edges[0] * 0.5 - xs[end_col]
                } else {
                    ox + x_edges[0] * 0.5 + xs[cell.col]
                };
                (cell_x, oy + y_edges[0] * 0.5 + ys[cell.row])
            } else {
                (
                    ox + inset
                        + if style.direction == Direction::Rtl {
                            width - spacing_x - xs[cell.col] - cw
                        } else {
                            spacing_x + xs[cell.col]
                        },
                    oy + inset + spacing_y + ys[cell.row],
                )
            };
            let mut computed = cell.style.resolve_percentages(
                (width - gaps - collapsed_edge_sum).max(0.0),
                self.parent_height,
            );
            computed.display = Display::Block;
            computed.margin = 0.0;
            computed.box_sizing = BoxSizing::BorderBox;
            if let (Some(horizontal), Some(vertical)) = (&collapsed_horizontal, &collapsed_vertical)
            {
                let top = collapsed_edge_width(horizontal, cell.row, cell.col, end_col) * 0.5;
                let right = collapsed_edge_width(vertical, end_col, cell.row, end_row) * 0.5;
                let bottom = collapsed_edge_width(horizontal, end_row, cell.col, end_col) * 0.5;
                let left = collapsed_edge_width(vertical, cell.col, cell.row, end_row) * 0.5;
                computed.border_width_sides = [Some(top), Some(right), Some(bottom), Some(left)];
                computed.border_solid_sides = [Some(true); 4];
                computed.border_width = top.max(right).max(bottom).max(left);
                computed.border_solid = true;
            }
            computed.width = Some(cw);
            computed.height = Some(ch);
            let aligns_at_baseline = matches!(
                cell.style.vertical_align,
                css::VerticalAlign::Baseline
                    | css::VerticalAlign::Sub
                    | css::VerticalAlign::Super
                    | css::VerticalAlign::TextTop
                    | css::VerticalAlign::TextBottom
                    | css::VerticalAlign::Length(_)
            );
            computed.table_cell_content_offset = match cell.style.vertical_align {
                css::VerticalAlign::Top => 0.0,
                css::VerticalAlign::Middle => ((ch - cell.height).max(0.0)) * 0.5,
                css::VerticalAlign::Bottom => (ch - cell.height).max(0.0),
                _ if aligns_at_baseline => {
                    let cell_baseline = cell.baseline_offset.unwrap_or(cell.height);
                    (baseline_ascent.get(cell.row).copied().unwrap_or(0.0) - cell_baseline).max(0.0)
                }
                _ => 0.0,
            };
            let saved_suppressed_border = self.suppressed_border_node;
            if style.border_collapse {
                self.suppressed_border_node = Some(cell.node);
            }
            let result = self.box_for(cell.node, style, Some(computed), cx, cy, cw, depth + 1);
            self.suppressed_border_node = saved_suppressed_border;
            result?;
        }
        if let (Some(horizontal), Some(vertical), Some(x_edges), Some(y_edges)) = (
            collapsed_horizontal.as_ref(),
            collapsed_vertical.as_ref(),
            collapsed_x_widths.as_ref(),
            collapsed_y_widths.as_ref(),
        ) {
            for (row, line) in horizontal.iter().enumerate() {
                for (col, candidate) in line.iter().enumerate() {
                    let Some(candidate) = candidate.filter(|candidate| {
                        candidate.color.a > 0
                            && candidate.width > 0.0
                            && candidate.style != BorderStyle::Hidden
                    }) else {
                        continue;
                    };
                    let x = if style.direction == Direction::Rtl {
                        ox + width - x_edges[0] * 0.5 - xs[col + 1]
                    } else {
                        ox + x_edges[0] * 0.5 + xs[col]
                    };
                    let y = oy + y_edges[0] * 0.5 + ys[row] - candidate.width * 0.5;
                    self.push_command(Command::FillRect {
                        rect: Rect {
                            x,
                            y,
                            width: xs[col + 1] - xs[col],
                            height: candidate.width,
                        },
                        color: candidate.color,
                    })?;
                }
            }
            for (col, line) in vertical.iter().enumerate() {
                let x = if style.direction == Direction::Rtl {
                    ox + width - x_edges[0] * 0.5 - xs[col]
                } else {
                    ox + x_edges[0] * 0.5 + xs[col]
                };
                for (row, candidate) in line.iter().enumerate() {
                    let Some(candidate) = candidate.filter(|candidate| {
                        candidate.color.a > 0
                            && candidate.width > 0.0
                            && candidate.style != BorderStyle::Hidden
                    }) else {
                        continue;
                    };
                    self.push_command(Command::FillRect {
                        rect: Rect {
                            x: x - candidate.width * 0.5,
                            y: oy + y_edges[0] * 0.5 + ys[row],
                            width: candidate.width,
                            height: ys[row + 1] - ys[row],
                        },
                        color: candidate.color,
                    })?;
                }
            }
        }
        if style.caption_bottom {
            if let Some((caption_node, mut caption_style)) = caption {
                caption_style.display = Display::Block;
                caption_style.margin = 0.0;
                caption_style.box_sizing = BoxSizing::BorderBox;
                caption_style.width = Some(width);
                caption_style.height = None;
                caption_height = self.box_for(
                    caption_node,
                    style,
                    Some(caption_style),
                    ox,
                    rect.y + rect.height,
                    width,
                    depth + 1,
                )?;
            }
        }
        Ok(rect.height + margin_top + margin_bottom + caption_height)
    }

    fn intrinsic_size(
        &self,
        id: NodeId,
        style: &Style,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        self.intrinsic_size_mode(id, style, depth, false)
    }

    fn intrinsic_size_mode(
        &self,
        id: NodeId,
        style: &Style,
        depth: usize,
        minimum: bool,
    ) -> Result<(f32, f32), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            let text = collapsed_text(value);
            if minimum
                && !matches!(
                    style.white_space,
                    css::WhiteSpace::NoWrap | css::WhiteSpace::Pre
                )
            {
                let mut width = 0.0f32;
                for word in text.split_ascii_whitespace() {
                    width = width.max(
                        self.text
                            .measure_styled(word, style.font_size, &style.font)
                            .map_err(|_| LayoutError::Text)?,
                    );
                }
                return Ok((width, self.line_height(style)));
            }
            return Ok((
                self.text
                    .measure_styled(text.trim_matches(' '), style.font_size, &style.font)
                    .map_err(|_| LayoutError::Text)?,
                self.line_height(style),
            ));
        }
        let NodeKind::Element {
            name,
            namespace,
            attributes,
        } = kind
        else {
            return Ok((0.0, 0.0));
        };
        let svg_root = *namespace == Namespace::Svg && crate::svg::local_name(name) == "svg";
        if *namespace != Namespace::Html && !svg_root {
            return Ok((0.0, 0.0));
        }
        let tag = crate::svg::local_name(name);
        if style.display == Display::None
            || (*namespace == Namespace::Html
                && matches!(
                    tag,
                    "head" | "style" | "script" | "template" | "meta" | "link"
                ))
        {
            return Ok((0.0, 0.0));
        }
        let edges = box_edges(style, true);
        let vertical_edges = box_edges(style, false);
        if svg_root {
            let (width, height) = crate::svg::root_size(
                attributes,
                style.width.map(|width| content_dimension(style, width)),
                style
                    .height
                    .map(|height| content_height_dimension(style, height)),
                self.viewport.width,
                Some(self.viewport.height),
            );
            return Ok((
                constrained_width(style, width) + edges,
                constrained_height(style, height) + vertical_edges,
            ));
        }
        // CSS Sizing's preferred aspect ratio supplies an automatic dimension
        // before flex/grid item sizing consumes the intrinsic contribution.
        // Applying it only in `box_content` is too late for a column flex item:
        // its padding would otherwise be mistaken for the entire main size.
        if let Some(ratio) = style
            .aspect_ratio
            .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
        {
            match (style.width, style.height) {
                (Some(width), None) => {
                    let content_width = constrained_width(style, content_dimension(style, width));
                    let content_height = constrained_height(style, content_width / ratio);
                    return Ok((content_width + edges, content_height + vertical_edges));
                }
                (None, Some(height)) => {
                    let content_height =
                        constrained_height(style, content_height_dimension(style, height));
                    let content_width = constrained_width(style, content_height * ratio);
                    return Ok((content_width + edges, content_height + vertical_edges));
                }
                _ => {}
            }
        }
        if let (Some(width), Some(height)) = (style.width, style.height) {
            return Ok((
                constrained_width(style, content_dimension(style, width)) + edges,
                content_height_dimension(style, height) + vertical_edges,
            ));
        }
        if style.contain_size {
            return Ok((
                style
                    .width
                    .map_or(0.0, |width| content_dimension(style, width))
                    + edges,
                style
                    .height
                    .map_or(0.0, |height| content_height_dimension(style, height))
                    + vertical_edges,
            ));
        }
        if tag == "img" || tag == "canvas" || tag == "video" {
            let dimension = |key: &str| {
                if tag == "canvas" {
                    let raw = crate::svg::attribute(attributes, key);
                    return Some(
                        canvas_dimension(raw, if key == "width" { 300 } else { 150 }) as f32,
                    );
                }
                crate::svg::attribute(attributes, key)
                    .and_then(|value| value.parse::<f32>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
            };
            let mut width = style
                .width
                .or_else(|| dimension("width"))
                .map(|value| content_dimension(style, value));
            let mut height = style
                .height
                .or_else(|| dimension("height"))
                .map(|value| content_height_dimension(style, value));
            if width.is_none() || height.is_none() {
                if let Some(source) = crate::svg::attribute(attributes, "src") {
                    let image = match self.images.and_then(|images| {
                        resolve_element_image(
                            images,
                            id,
                            Some(source),
                            self.rules.document_base_url(),
                        )
                    }) {
                        Some(ImageState::Ready(image)) if image.is_valid() => image,
                        Some(ImageState::Pending) => return Err(LayoutError::ImagePending),
                        _ => return Err(LayoutError::ImageFailed),
                    };
                    match (width, height) {
                        (Some(w), None) => {
                            height = Some(w * image.height as f32 / image.width as f32)
                        }
                        (None, Some(h)) => {
                            width = Some(h * image.width as f32 / image.height as f32)
                        }
                        _ => {
                            width = Some(image.width as f32);
                            height = Some(image.height as f32);
                        }
                    }
                }
            }
            return Ok((
                constrained_width(style, width.unwrap_or(0.0)) + edges,
                height.unwrap_or(0.0) + vertical_edges,
            ));
        }
        let slot = id.index() * 2 + usize::from(minimum);
        if let Some(Some((cached_style, size))) = self.intrinsic_cache.borrow().get(slot) {
            if cached_style == style {
                return Ok(*size);
            }
        }
        let (mut width, mut height, mut inline_width, mut inline_height) =
            (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut has_children = false;
        let mut children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            has_children = true;
            let child_kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let element_style;
            let child_style = if matches!(child_kind, NodeKind::Element { .. }) {
                element_style = self
                    .computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?;
                &element_style
            } else {
                style
            };
            let (w, h) = self.intrinsic_size_mode(node, child_style, depth + 1, minimum)?;
            if matches!(child_kind, NodeKind::Text(_)) || child_style.display == Display::Inline {
                inline_width += w;
                inline_height = inline_height.max(h);
            } else {
                width = width.max(inline_width).max(w);
                height += inline_height + h;
                inline_width = 0.0;
                inline_height = 0.0;
            }
        }
        let size = (
            constrained_width(
                style,
                style
                    .width
                    .map(|value| content_dimension(style, value))
                    .unwrap_or(width.max(inline_width)),
            ) + edges,
            style
                .height
                .map(|value| content_height_dimension(style, value))
                .unwrap_or(height + inline_height)
                + vertical_edges,
        );
        if has_children {
            let mut cache = self.intrinsic_cache.borrow_mut();
            if cache.len() <= slot {
                cache.resize_with(slot + 1, || None);
            }
            cache[slot] = Some((style.clone(), size));
        }
        Ok(size)
    }

    fn grid_children(
        &mut self,
        parent: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        width: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        use css::{GridTrack, MAX_GRID_TRACKS};
        struct Item {
            node: NodeId,
            style: Style,
            generated: Option<VirtualGeneratedChild>,
            anonymous_text: bool,
            source_order: usize,
            col: usize,
            row: usize,
            cols: usize,
            rows: usize,
            natural: (f32, f32),
            minimum: f32,
            commands: core::ops::Range<usize>,
            hits: core::ops::Range<usize>,
        }
        let mut items = Vec::new();
        let mut positioned = Vec::new();
        let generated_before =
            self.virtual_generated_child(parent, style, css::PseudoElement::Before, width)?;
        let generated_after =
            self.virtual_generated_child(parent, style, css::PseudoElement::After, width)?;
        let mut source_order = usize::from(generated_before.is_some());
        // Auto-repeat expands before placement: the repetition count comes from
        // the definite available size (CSS Grid §7.2.3.2); indefinite → 1.
        let definite_height = style
            .height
            .map(|height| content_height_dimension(style, height))
            .filter(|height| height.is_finite() && *height >= 0.0);
        let row_gap = resolved_gap(style.gap, style.row_gap_fraction, definite_height);
        let column_gap = resolved_gap(
            style.column_gap.unwrap_or(0.0),
            style.column_gap_fraction,
            Some(width),
        );
        let mut column_tracks_storage: Vec<GridTrack> = Vec::new();
        let mut row_tracks_storage: Vec<GridTrack> = Vec::new();
        let mut column_names_storage = style.grid_column_names.as_deref().unwrap_or(&[]).to_vec();
        let mut row_names_storage = style.grid_row_names.as_deref().unwrap_or(&[]).to_vec();
        let column_auto = style.grid_columns_auto.clone();
        let row_auto = style.grid_rows_auto.clone();
        let mut column_auto_count = 0usize;
        let mut row_auto_count = 0usize;
        if let Some(auto) = &column_auto {
            let available = Some(width).filter(|v| v.is_finite() && *v >= 0.0);
            column_auto_count = auto_repeat_count(auto, available, column_gap)?;
            (column_tracks_storage, column_names_storage) =
                expand_auto_repeat(auto, column_auto_count)?;
        }
        if let Some(auto) = &row_auto {
            let available = style
                .height
                .map(|v| content_height_dimension(style, v))
                .filter(|v| v.is_finite() && *v >= 0.0);
            row_auto_count = auto_repeat_count(auto, available, row_gap)?;
            (row_tracks_storage, row_names_storage) = expand_auto_repeat(auto, row_auto_count)?;
        }
        if column_tracks_storage.len() > MAX_GRID_TRACKS
            || row_tracks_storage.len() > MAX_GRID_TRACKS
        {
            return Err(LayoutError::GridLimit);
        }
        let explicit_column_count = if column_auto.is_some() {
            column_tracks_storage.len()
        } else {
            style.grid_columns.as_deref().map_or(0, |v| v.len())
        };
        let explicit_row_count = if row_auto.is_some() {
            row_tracks_storage.len()
        } else {
            style.grid_rows.as_deref().map_or(0, |v| v.len())
        };
        let mut columns = explicit_column_count.max(1);
        let mut rows = explicit_row_count.max(1);
        if let Some(areas) = &style.grid_areas {
            for area in areas.iter() {
                columns = columns.max(area.column + area.columns);
                rows = rows.max(area.row + area.rows);
            }
        }
        let mut children = self
            .document
            .composed_children_iter(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            let kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let anonymous_text = match &kind {
                NodeKind::Text(value) => !collapsed_text(value).trim_matches(' ').is_empty(),
                _ => false,
            };
            if matches!(kind, NodeKind::Element { .. }) || anonymous_text {
                let mut computed = self
                    .computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?;
                if computed.display != Display::None {
                    if matches!(computed.position, Position::Absolute | Position::Fixed) {
                        if positioned.len() >= MAX_DISPLAY_COMMANDS {
                            return Err(LayoutError::CommandLimit);
                        }
                        positioned
                            .try_reserve(1)
                            .map_err(|_| LayoutError::CommandLimit)?;
                        positioned.push((node, computed));
                        continue;
                    }
                    if let Some(area) = computed.grid_area.as_ref().and_then(|name| {
                        style
                            .grid_areas
                            .as_ref()?
                            .iter()
                            .find(|area| area.name == *name)
                    }) {
                        computed.grid_column = css::GridPlacement {
                            start: Some(area.column),
                            span: area.columns,
                        };
                        computed.grid_row = css::GridPlacement {
                            start: Some(area.row),
                            span: area.rows,
                        };
                    }
                    computed.grid_column = css::resolve_grid_placement(
                        computed.grid_column,
                        computed.grid_column_spec.as_deref(),
                        &column_names_storage,
                        explicit_column_count,
                        style.grid_areas.as_deref().unwrap_or(&[]),
                        true,
                    )
                    .ok_or(LayoutError::GridLimit)?;
                    computed.grid_row = css::resolve_grid_placement(
                        computed.grid_row,
                        computed.grid_row_spec.as_deref(),
                        &row_names_storage,
                        explicit_row_count,
                        style.grid_areas.as_deref().unwrap_or(&[]),
                        false,
                    )
                    .ok_or(LayoutError::GridLimit)?;
                    if items.len() == MAX_GRID_TRACKS * MAX_GRID_TRACKS {
                        return Err(LayoutError::GridLimit);
                    }
                    columns = columns
                        .max(computed.grid_column.start.unwrap_or(0) + computed.grid_column.span);
                    rows = rows.max(computed.grid_row.start.unwrap_or(0) + computed.grid_row.span);
                    if columns > MAX_GRID_TRACKS || rows > MAX_GRID_TRACKS {
                        return Err(LayoutError::GridLimit);
                    }
                    let sizing_style = computed.resolve_percentages(width, self.parent_height);
                    let natural = self.intrinsic_size(node, &sizing_style, depth + 1)?;
                    let minimum = self
                        .intrinsic_size_mode(node, &sizing_style, depth + 1, true)?
                        .0;
                    items.push(Item {
                        node,
                        style: computed,
                        generated: None,
                        anonymous_text,
                        source_order,
                        col: 0,
                        row: 0,
                        cols: 1,
                        rows: 1,
                        natural,
                        minimum,
                        commands: 0..0,
                        hits: 0..0,
                    });
                    source_order += 1;
                }
            }
        }
        for child in [generated_before, generated_after].into_iter().flatten() {
            let pseudo_source_order = if child.pseudo == css::PseudoElement::Before {
                0
            } else {
                source_order
            };
            let mut computed = child.style.clone();
            if computed.position != Position::Static
                || computed.grid_columns_subgrid
                || computed.grid_rows_subgrid
            {
                return Err(LayoutError::UnsupportedGeneratedContent);
            }
            computed.float = Float::None;
            computed.clear = Clear::None;
            if let Some(area) = computed.grid_area.as_ref().and_then(|name| {
                style
                    .grid_areas
                    .as_ref()?
                    .iter()
                    .find(|area| area.name == *name)
            }) {
                computed.grid_column = css::GridPlacement {
                    start: Some(area.column),
                    span: area.columns,
                };
                computed.grid_row = css::GridPlacement {
                    start: Some(area.row),
                    span: area.rows,
                };
            }
            computed.grid_column = css::resolve_grid_placement(
                computed.grid_column,
                computed.grid_column_spec.as_deref(),
                &column_names_storage,
                explicit_column_count,
                style.grid_areas.as_deref().unwrap_or(&[]),
                true,
            )
            .ok_or(LayoutError::GridLimit)?;
            computed.grid_row = css::resolve_grid_placement(
                computed.grid_row,
                computed.grid_row_spec.as_deref(),
                &row_names_storage,
                explicit_row_count,
                style.grid_areas.as_deref().unwrap_or(&[]),
                false,
            )
            .ok_or(LayoutError::GridLimit)?;
            if items.len() == MAX_GRID_TRACKS * MAX_GRID_TRACKS {
                return Err(LayoutError::GridLimit);
            }
            columns =
                columns.max(computed.grid_column.start.unwrap_or(0) + computed.grid_column.span);
            rows = rows.max(computed.grid_row.start.unwrap_or(0) + computed.grid_row.span);
            if columns > MAX_GRID_TRACKS || rows > MAX_GRID_TRACKS {
                return Err(LayoutError::GridLimit);
            }
            let sizing_style = computed.resolve_percentages(width, self.parent_height);
            let natural = self.intrinsic_virtual_generated_size(&child, &sizing_style, false)?;
            let minimum = self
                .intrinsic_virtual_generated_size(&child, &sizing_style, true)?
                .0;
            items
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            items.push(Item {
                node: child.origin,
                style: computed,
                generated: Some(child),
                anonymous_text: false,
                source_order: pseudo_source_order,
                col: 0,
                row: 0,
                cols: 1,
                rows: 1,
                natural,
                minimum,
                commands: 0..0,
                hits: 0..0,
            });
        }
        items.sort_by_key(|item| (item.style.order, item.source_order));
        let mut occupied = [0u64; MAX_GRID_TRACKS];
        let mut cursor = 0;
        let column_flow = style.grid_auto_flow.column;
        let flow_rows = rows;
        let flow_columns = columns;
        // Reserve definite areas, then major-axis locked items, then remaining auto items.
        for phase in 0..3 {
            for item in &mut items {
                let cp = item.style.grid_column;
                let rp = item.style.grid_row;
                let definite = cp.start.is_some() && rp.start.is_some();
                let locked = if column_flow {
                    cp.start.is_some()
                } else {
                    rp.start.is_some()
                };
                if (if definite {
                    0
                } else if locked {
                    1
                } else {
                    2
                }) != phase
                {
                    continue;
                }
                item.cols = cp.span;
                item.rows = rp.span;
                let mut found = None;
                for index in if phase < 2 || style.grid_auto_flow.dense {
                    0
                } else {
                    cursor
                }
                    ..MAX_GRID_TRACKS * if column_flow { flow_rows } else { flow_columns }
                {
                    let mut col = cp.start.unwrap_or(if column_flow {
                        index / flow_rows
                    } else {
                        index % flow_columns
                    });
                    let mut row = rp.start.unwrap_or(if column_flow {
                        index % flow_rows
                    } else {
                        index / flow_columns
                    });
                    if phase == 2 && !style.grid_auto_flow.dense {
                        if !column_flow
                            && cp.start.is_some_and(|start| start < index % flow_columns)
                        {
                            row += 1;
                        }
                        if column_flow && rp.start.is_some_and(|start| start < index % flow_rows) {
                            col += 1;
                        }
                    }
                    if col + item.cols
                        > if column_flow {
                            MAX_GRID_TRACKS
                        } else {
                            flow_columns
                        }
                        || row + item.rows
                            > if column_flow {
                                flow_rows
                            } else {
                                MAX_GRID_TRACKS
                            }
                    {
                        continue;
                    }
                    let mask = (u64::MAX >> (64 - item.cols)) << col;
                    if definite || occupied[row..row + item.rows].iter().all(|v| v & mask == 0) {
                        found = Some((col, row, mask));
                        break;
                    }
                }
                let (col, row, mask) = found.ok_or(LayoutError::GridLimit)?;
                for value in &mut occupied[row..row + item.rows] {
                    *value |= mask;
                }
                item.col = col;
                item.row = row;
                rows = rows.max(row + item.rows);
                columns = columns.max(col + item.cols);
                if phase == 2 {
                    cursor = if column_flow {
                        col * flow_rows + row + item.rows
                    } else {
                        row * flow_columns + col + item.cols
                    };
                }
            }
        }
        // auto-fit collapses empty repeated tracks after placement; surviving
        // items shift by the number of collapsed tracks before their start.
        if let Some(auto) = &column_auto {
            if auto.fit && column_auto_count > 0 {
                let repeat_start = auto.prefix_tracks.len();
                let repeat_track_count = column_auto_count * auto.tracks.len();
                let repeat_end = repeat_start + repeat_track_count;
                let removed: Vec<usize> = (repeat_start..repeat_end)
                    .filter(|&track| {
                        !items
                            .iter()
                            .any(|item| item.col < track + 1 && item.col + item.cols > track)
                    })
                    .collect();
                if !removed.is_empty() {
                    let collapsed_before =
                        |start: usize| removed.iter().filter(|track| **track < start).count();
                    for item in &mut items {
                        item.col -= collapsed_before(item.col);
                    }
                    column_tracks_storage = column_tracks_storage
                        .iter()
                        .enumerate()
                        .filter(|(track, _)| !removed.contains(track))
                        .map(|(_, track)| *track)
                        .collect();
                    columns -= removed.len();
                    for line in &mut column_names_storage {
                        line.line -= collapsed_before(line.line);
                    }
                }
            }
        }
        if let Some(auto) = &row_auto {
            if auto.fit && row_auto_count > 0 {
                let repeat_start = auto.prefix_tracks.len();
                let repeat_track_count = row_auto_count * auto.tracks.len();
                let repeat_end = repeat_start + repeat_track_count;
                let removed: Vec<usize> = (repeat_start..repeat_end)
                    .filter(|&track| {
                        !items
                            .iter()
                            .any(|item| item.row < track + 1 && item.row + item.rows > track)
                    })
                    .collect();
                if !removed.is_empty() {
                    let collapsed_before =
                        |start: usize| removed.iter().filter(|track| **track < start).count();
                    for item in &mut items {
                        item.row -= collapsed_before(item.row);
                    }
                    row_tracks_storage = row_tracks_storage
                        .iter()
                        .enumerate()
                        .filter(|(track, _)| !removed.contains(track))
                        .map(|(_, track)| *track)
                        .collect();
                    rows -= removed.len();
                    for line in &mut row_names_storage {
                        line.line -= collapsed_before(line.line);
                    }
                }
            }
        }
        let column_tracks: &[GridTrack] = if column_auto.is_some() {
            &column_tracks_storage
        } else {
            style.grid_columns.as_deref().unwrap_or(&[])
        };
        let row_tracks: &[GridTrack] = if row_auto.is_some() {
            &row_tracks_storage
        } else {
            style.grid_rows.as_deref().unwrap_or(&[])
        };
        let mut column_definitions = [GridTrack::Auto; MAX_GRID_TRACKS];
        let mut row_definitions = [GridTrack::Auto; MAX_GRID_TRACKS];
        for (definitions, explicit, implicit, count) in [
            (
                &mut column_definitions,
                column_tracks,
                style.grid_auto_columns.as_deref().unwrap_or(&[]),
                columns,
            ),
            (
                &mut row_definitions,
                row_tracks,
                style.grid_auto_rows.as_deref().unwrap_or(&[]),
                rows,
            ),
        ] {
            for i in 0..count {
                definitions[i] = explicit.get(i).copied().unwrap_or_else(|| {
                    if implicit.is_empty() {
                        GridTrack::Auto
                    } else {
                        implicit[(i - explicit.len()) % implicit.len()]
                    }
                });
            }
        }
        let tracks = |items: &[Item],
                      definitions: &[GridTrack],
                      count: usize,
                      available: Option<f32>,
                      horizontal: bool| {
            grid_track_sizes(
                definitions,
                count,
                available,
                if horizontal { column_gap } else { row_gap },
                if horizontal {
                    style.justify_content == JustifyContent::Stretch
                } else {
                    style.align_content.is_none()
                },
                items.iter().map(|item| {
                    if horizontal {
                        (item.col, item.cols, item.minimum, item.natural.0)
                    } else {
                        (item.row, item.rows, item.natural.1, item.natural.1)
                    }
                }),
            )
        };
        let widths = tracks(&items, &column_definitions, columns, Some(width), true);
        let mut row_contributions = Vec::new();
        let mut subgrid_contributions = Vec::new();
        for item in &items {
            if !item.style.grid_rows_subgrid {
                row_contributions.push((item.row, item.rows, item.natural.1, item.natural.1));
                continue;
            }
            let mut subgrid_row_names: Vec<css::GridNamedLine> = row_names_storage
                .iter()
                .filter(|line| line.line >= item.row && line.line <= item.row + item.rows)
                .map(|line| css::GridNamedLine {
                    name: line.name.clone(),
                    line: line.line - item.row,
                })
                .collect();
            subgrid_row_names.extend(
                item.style
                    .grid_row_names
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|line| line.line <= item.rows)
                    .cloned(),
            );
            let mut nested = self
                .document
                .composed_children_iter(item.node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let mut auto_index = 0usize;
            while let Some(node) = nested.next().map_err(|_| LayoutError::InvalidTree)? {
                let kind = self
                    .document
                    .kind(node)
                    .map_err(|_| LayoutError::InvalidTree)?;
                let anonymous_text = match &kind {
                    NodeKind::Text(value) => !collapsed_text(value).trim_matches(' ').is_empty(),
                    _ => false,
                };
                if !matches!(kind, NodeKind::Element { .. }) && !anonymous_text {
                    continue;
                }
                let child = self
                    .computed_style(node, Some(&item.style))
                    .map_err(LayoutError::Css)?;
                if child.display == Display::None
                    || matches!(child.position, Position::Absolute | Position::Fixed)
                {
                    continue;
                }
                let placement = css::resolve_grid_placement(
                    child.grid_row,
                    child.grid_row_spec.as_deref(),
                    &subgrid_row_names,
                    item.rows,
                    &[],
                    false,
                )
                .ok_or(LayoutError::GridLimit)?;
                let row = placement.start.unwrap_or(auto_index / item.cols.max(1));
                let span = placement.span.max(1);
                auto_index += 1;
                if item.row + row >= rows || item.row + row + span > MAX_GRID_TRACKS {
                    continue;
                }
                let natural = self.intrinsic_size(node, &child, depth + 1)?.1;
                row_contributions.push((item.row + row, span, natural, natural));
                subgrid_contributions.push((item.row + row, span, natural, natural));
            }
        }
        let distribute = |count: usize, free: f32, alignment: JustifyContent| match alignment {
            JustifyContent::End => (free, 0.0),
            JustifyContent::Center => (free * 0.5, 0.0),
            JustifyContent::SpaceBetween if count > 1 => (0.0, free / (count - 1) as f32),
            JustifyContent::SpaceAround if count > 0 => {
                (free / count as f32 * 0.5, free / count as f32)
            }
            JustifyContent::SpaceEvenly => (free / (count + 1) as f32, free / (count + 1) as f32),
            _ => (0.0, 0.0),
        };
        let (column_offset, column_extra) = distribute(
            columns,
            (width
                - widths[..columns].iter().sum::<f32>()
                - column_gap * columns.saturating_sub(1) as f32)
                .max(0.0),
            style.justify_content,
        );
        let preliminary_rows = grid_track_sizes(
            &row_definitions,
            rows,
            style.height.map(|v| content_height_dimension(style, v)),
            row_gap,
            style.align_content.is_none(),
            row_contributions.iter().copied(),
        );
        // Lay out once at the final column width; retain commands and move them after row sizing.
        for item in &mut items {
            let mut cx = x
                + column_offset
                + widths[..item.col].iter().sum::<f32>()
                + (column_gap + column_extra) * item.col as f32;
            let cw = widths[item.col..item.col + item.cols].iter().sum::<f32>()
                + (column_gap + column_extra) * (item.cols - 1) as f32;
            if style.direction == css::Direction::Rtl {
                cx = x + width - (cx - x) - cw;
            }
            let definite_height = style.height.is_some()
                || row_definitions[item.row..item.row + item.rows]
                    .iter()
                    .all(|track| matches!(track, GridTrack::Pixels(_)));
            let area_height = preliminary_rows[item.row..item.row + item.rows]
                .iter()
                .sum::<f32>()
                + row_gap * (item.rows - 1) as f32;
            let mut computed = item
                .style
                .resolve_percentages(cw, definite_height.then_some(area_height));
            if computed.grid_columns_subgrid || computed.grid_rows_subgrid {
                let subrow_gap = if computed.gap_specified {
                    resolved_gap(
                        computed.gap,
                        computed.row_gap_fraction,
                        definite_height.then_some(area_height),
                    )
                } else {
                    row_gap
                };
                let subcolumn_gap = resolved_gap(
                    computed.column_gap.unwrap_or(column_gap),
                    computed.column_gap_fraction,
                    Some(cw),
                );
                let row_difference = subrow_gap - row_gap;
                let column_difference = subcolumn_gap - column_gap;
                let border = if computed.border_solid {
                    computed.border_width
                } else {
                    0.0
                };
                if computed.grid_columns_subgrid {
                    let mut shared: Vec<_> = widths[item.col..item.col + item.cols]
                        .iter()
                        .copied()
                        .collect();
                    for (index, size) in shared.iter_mut().enumerate() {
                        let start = if index == 0 {
                            computed.padding_sides[3] + border
                        } else {
                            column_difference * 0.5
                        };
                        let end = if index + 1 == item.cols {
                            computed.padding_sides[1] + border
                        } else {
                            column_difference * 0.5
                        };
                        *size = (*size - start - end).max(0.0);
                    }
                    computed.grid_columns = Some(
                        shared
                            .into_iter()
                            .map(GridTrack::Pixels)
                            .collect::<Vec<_>>()
                            .into(),
                    );
                    let mut names: Vec<_> = column_names_storage
                        .iter()
                        .filter(|line| line.line >= item.col && line.line <= item.col + item.cols)
                        .map(|line| css::GridNamedLine {
                            name: line.name.clone(),
                            line: line.line - item.col,
                        })
                        .collect();
                    names.extend(
                        computed
                            .grid_column_names
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .filter(|line| line.line <= item.cols)
                            .cloned(),
                    );
                    computed.grid_column_names = (!names.is_empty()).then(|| names.into());
                }
                if computed.grid_rows_subgrid {
                    let mut shared: Vec<_> = preliminary_rows[item.row..item.row + item.rows]
                        .iter()
                        .copied()
                        .collect();
                    for (index, size) in shared.iter_mut().enumerate() {
                        let start = if index == 0 {
                            computed.padding_sides[0] + border
                        } else {
                            row_difference * 0.5
                        };
                        let end = if index + 1 == item.rows {
                            computed.padding_sides[2] + border
                        } else {
                            row_difference * 0.5
                        };
                        *size = (*size - start - end).max(0.0);
                    }
                    computed.grid_rows = Some(
                        shared
                            .into_iter()
                            .map(GridTrack::Pixels)
                            .collect::<Vec<_>>()
                            .into(),
                    );
                    let mut names: Vec<_> = row_names_storage
                        .iter()
                        .filter(|line| line.line >= item.row && line.line <= item.row + item.rows)
                        .map(|line| css::GridNamedLine {
                            name: line.name.clone(),
                            line: line.line - item.row,
                        })
                        .collect();
                    names.extend(
                        computed
                            .grid_row_names
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .filter(|line| line.line <= item.rows)
                            .cloned(),
                    );
                    computed.grid_row_names = (!names.is_empty()).then(|| names.into());
                    if computed.height.is_none() && computed.height_intrinsic.is_none() {
                        computed.height = Some(specified_height(
                            &computed,
                            (area_height - box_edges(&computed, false)).max(0.0),
                        ));
                    }
                }
                computed.gap = subrow_gap;
                computed.row_gap_fraction = 0.0;
                computed.column_gap = Some(subcolumn_gap);
                computed.column_gap_fraction = 0.0;
            }
            if computed.display == Display::Inline {
                computed.display = Display::Block;
            }
            let margin = computed.margin_sides[1] + computed.margin_sides[3];
            let alignment = computed.justify_self.unwrap_or(style.justify_items);
            if computed.width.is_none() {
                computed.width = Some(specified_dimension(
                    &computed,
                    ((if alignment == AlignItems::Stretch {
                        cw
                    } else {
                        item.natural.0.min(cw).max(item.minimum)
                    }) - margin
                        - box_edges(&computed, true))
                    .max(0.0),
                ));
            }
            let free = (cw
                - margin
                - content_dimension(&computed, computed.width.unwrap_or(0.0))
                - box_edges(&computed, true))
            .max(0.0);
            cx += if computed.margin_auto[3] {
                if computed.margin_auto[1] {
                    free * 0.5
                } else {
                    free
                }
            } else if computed.margin_auto[1] {
                0.0
            } else {
                match alignment {
                    AlignItems::End => {
                        if style.direction == css::Direction::Rtl {
                            0.0
                        } else {
                            free
                        }
                    }
                    AlignItems::Start | AlignItems::Stretch | AlignItems::Baseline => {
                        if style.direction == css::Direction::Rtl {
                            free
                        } else {
                            0.0
                        }
                    }
                    AlignItems::Center => free * 0.5,
                }
            };
            item.commands.start = self.commands.len();
            item.hits.start = self.geometry.as_ref().map_or(0, |g| g.hits.len());
            item.natural.1 = if let Some(generated) = item.generated.as_ref() {
                self.box_for_virtual_generated(
                    generated,
                    style,
                    &computed,
                    cx,
                    y,
                    cw,
                    &[],
                    depth + 1,
                )?
            } else if item.anonymous_text {
                self.box_for(item.node, &computed, None, cx, y, cw, depth + 1)?
            } else {
                self.box_for(item.node, style, Some(computed), cx, y, cw, depth + 1)?
            };
            item.commands.end = self.commands.len();
            item.hits.end = self.geometry.as_ref().map_or(0, |g| g.hits.len());
        }
        let heights = grid_track_sizes(
            &row_definitions,
            rows,
            style.height.map(|v| content_height_dimension(style, v)),
            row_gap,
            style.align_content.is_none(),
            items
                .iter()
                .filter(|item| !item.style.grid_rows_subgrid)
                .map(|item| (item.row, item.rows, item.natural.1, item.natural.1))
                .chain(subgrid_contributions.iter().copied()),
        );
        // Percentage row gaps are cyclic while an auto-height grid is being
        // sized, so they contribute zero to the intrinsic height. Once that
        // size is known, place the tracks using the resolved gap; it may
        // overflow the intrinsic content size, as CSS Grid requires.
        let used_height =
            heights[..rows].iter().sum::<f32>() + row_gap * rows.saturating_sub(1) as f32;
        let placement_row_gap = if definite_height.is_some() {
            row_gap
        } else {
            resolved_gap(style.gap, style.row_gap_fraction, Some(used_height))
        };
        let (row_offset, row_extra) = distribute(
            rows,
            (style
                .height
                .map_or(used_height, |v| content_height_dimension(style, v))
                - used_height)
                .max(0.0),
            style.align_content.unwrap_or(JustifyContent::Stretch),
        );
        for item in &items {
            let ch = heights[item.row..item.row + item.rows].iter().sum::<f32>()
                + (placement_row_gap + row_extra) * (item.rows - 1) as f32;
            let free = (ch - item.natural.1).max(0.0);
            let alignment = item.style.align_self.unwrap_or(style.align_items);
            let align = if item.style.margin_auto[0] {
                if item.style.margin_auto[2] {
                    free * 0.5
                } else {
                    free
                }
            } else if item.style.margin_auto[2] {
                0.0
            } else {
                match alignment {
                    AlignItems::End => free,
                    AlignItems::Center => free * 0.5,
                    _ => 0.0,
                }
            };
            let dy = row_offset
                + heights[..item.row].iter().sum::<f32>()
                + (placement_row_gap + row_extra) * item.row as f32
                + align;
            if item.style.height.is_none()
                && item.style.height_intrinsic.is_none()
                && alignment == AlignItems::Stretch
                && !item.style.margin_auto[0]
                && !item.style.margin_auto[2]
            {
                let height =
                    (ch - item.style.margin_sides[0] - item.style.margin_sides[2]).max(0.0);
                let paints = if has_background(&item.style) {
                    background_paints(&item.style)
                } else {
                    0
                } + usize::from(
                    item.style.border_solid
                        && item.style.border_width > 0.0
                        && item.style.border_color.a != 0,
                );
                let mut remaining = paints;
                let mut layers = background_layers(&item.style).map(|layers| {
                    layers
                        .into_iter()
                        .enumerate()
                        .rev()
                        .filter(|(_, layer)| !matches!(layer.image, BackgroundImage::None))
                        .collect::<Vec<_>>()
                        .into_iter()
                });
                let mut command_bytes = self.command_bytes;
                for command in self.commands[item.commands.clone()].iter_mut().take(
                    paints
                        + usize::from(item.style.opacity < 1.0)
                        + usize::from(item.style.transforms.is_some()),
                ) {
                    match command {
                        Command::PushLayer { .. } | Command::PushTransform(_) => {}
                        Command::MaskedBackground(mask) if remaining > 0 => {
                            let layer = if matches!(*mask.paint, Command::FillBackground(_)) {
                                layers.as_mut().and_then(|layers| layers.next())
                            } else {
                                None
                            };
                            stretch_background_mask(
                                command,
                                &item.style,
                                height,
                                layer,
                                self.images,
                                self.viewport,
                                &mut command_bytes,
                            )?;
                            remaining -= 1;
                        }
                        Command::StrokeBoxBorder(border) if remaining > 0 => {
                            border.rect.height = height;
                            border.corners =
                                clipped_corners(border.rect, border.rect, &item.style).map(|r| *r);
                            remaining -= 1;
                        }
                        Command::FillRect { rect, .. }
                        | Command::StrokeBorder { rect, .. }
                        | Command::StrokePatternBorder { rect, .. }
                            if remaining > 0 =>
                        {
                            rect.height = height;
                            remaining -= 1;
                        }
                        Command::FillRoundedRect { rect, .. } if remaining > 0 => {
                            let inset = background_bleed_inset(&item.style);
                            let outer = Rect {
                                x: rect.x - inset,
                                y: rect.y - inset,
                                width: rect.width + 2.0 * inset,
                                height,
                            };
                            replace_command_bytes(
                                command,
                                &mut command_bytes,
                                background_color(outer, &item.style),
                            )?;
                            remaining -= 1;
                        }
                        Command::FillBackground(fill) if remaining > 0 => {
                            let rect = &fill.rect;
                            if let Some((layer_index, layer)) =
                                layers.as_mut().and_then(|layers| layers.next())
                            {
                                let border = used_border(&item.style);
                                let outer = Rect {
                                    x: rect.x - border,
                                    y: rect.y - border,
                                    width: rect.width + 2.0 * border,
                                    height,
                                };
                                let paint = background_paint(&layer, self.images);
                                replace_command_bytes(
                                    command,
                                    &mut command_bytes,
                                    background_command(
                                        outer,
                                        &item.style,
                                        &layer,
                                        layer_index,
                                        &paint,
                                        self.viewport,
                                        None,
                                    ),
                                )?;
                            }
                            remaining -= 1;
                        }
                        Command::BoxShadow {
                            rect,
                            corners,
                            shadow,
                            ..
                        } if remaining > 0 => {
                            stretch_shadow(rect, corners, shadow.inset, height, &item.style);
                            remaining -= 1;
                        }
                        _ => break,
                    }
                }
                self.command_bytes = command_bytes;
                if let Some(geometry) = self.geometry.as_mut() {
                    if let Some(hit) = geometry.hits.get_mut(item.hits.start) {
                        if hit.node == item.node {
                            hit.rect.height = height;
                        }
                    }
                }
            }
            self.refresh_transform(&item.style, item.commands.clone(), item.hits.clone())?;
            for command in &mut self.commands[item.commands.clone()] {
                move_command(command, 0.0, dy);
            }
            if let Some(geometry) = self.geometry.as_mut() {
                geometry.move_hits(item.hits.clone(), 0.0, dy);
            }
        }
        for (node, child_style) in positioned {
            self.box_for(node, style, Some(child_style), x, y, width, depth + 1)?;
        }
        Ok((
            widths[..columns].iter().sum::<f32>() + column_gap * columns.saturating_sub(1) as f32,
            heights[..rows].iter().sum::<f32>() + row_gap * rows.saturating_sub(1) as f32,
        ))
    }

    fn paint_positioned_flex_children(
        &mut self,
        positioned: Vec<(NodeId, Style, f32, f32)>,
        style: &Style,
        x: f32,
        y: f32,
        width: f32,
        row: bool,
        reverse: bool,
        depth: usize,
    ) -> Result<(), LayoutError> {
        for (node, item_style, w, h) in positioned {
            let main_extent = if row {
                width
            } else {
                style
                    .height
                    .map_or(0.0, |height| content_height_dimension(style, height))
            };
            let cross_extent = if row {
                style
                    .height
                    .map_or(0.0, |height| content_height_dimension(style, height))
            } else {
                width
            };
            let main_size = if row { w } else { h };
            let cross_size = if row { h } else { w };
            let free = main_extent - main_size;
            let main = match style.justify_content {
                JustifyContent::End => {
                    if reverse {
                        0.0
                    } else {
                        free
                    }
                }
                JustifyContent::Center
                | JustifyContent::SpaceAround
                | JustifyContent::SpaceEvenly => free * 0.5,
                _ => {
                    if reverse {
                        free
                    } else {
                        0.0
                    }
                }
            };
            let cross = match item_style.align_self.unwrap_or(style.align_items) {
                AlignItems::Center => (cross_extent - cross_size) * 0.5,
                AlignItems::End => cross_extent - cross_size,
                _ => 0.0,
            };
            self.box_for(
                node,
                style,
                Some(item_style),
                x + if row { main } else { cross },
                y + if row { cross } else { main },
                width,
                depth + 1,
            )?;
        }
        Ok(())
    }

    fn virtual_flex_item(
        &self,
        child: VirtualGeneratedChild,
        container_style: &Style,
        row: bool,
        width: f32,
        source_order: usize,
        depth: usize,
    ) -> Result<FlexItem, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        if child.style.position != Position::Static {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }
        let mut item_style = child
            .unresolved_style
            .resolve_percentages(width, self.parent_height);
        item_style.float = Float::None;
        item_style.clear = Clear::None;
        let basis = child
            .unresolved_style
            .resolve_flex_basis_percentage(if row { Some(width) } else { self.parent_height });
        item_style.flex_basis = basis;

        let natural = self.intrinsic_virtual_generated_size(&child, &item_style, false)?;
        let minimum_intrinsic = self.intrinsic_virtual_generated_size(&child, &item_style, true)?;
        let edges = box_edges(&item_style, row);
        let cross_edges = box_edges(&item_style, !row);
        let content_basis = if item_style.flex_basis_content {
            let mut content_style = item_style.clone();
            if row {
                content_style.width = None;
            } else {
                content_style.height = None;
            }
            let content_size =
                self.intrinsic_virtual_generated_size(&child, &content_style, false)?;
            Some(if row { content_size.0 } else { content_size.1 })
        } else {
            None
        };
        let intrinsic_content_basis = if let Some(kind) = item_style.flex_basis_intrinsic {
            let mut sizing_style = item_style.clone();
            if row {
                sizing_style.width = None;
            } else {
                sizing_style.height = None;
            }
            sizing_style.flex_basis = None;
            sizing_style.flex_basis_content = false;
            sizing_style.flex_basis_intrinsic = None;
            let minimum = self.intrinsic_virtual_generated_size(&child, &sizing_style, true)?;
            let min_content = if row { minimum.0 } else { minimum.1 };
            match kind {
                css::IntrinsicSizing::MinContent => Some(min_content),
                css::IntrinsicSizing::MaxContent => {
                    let maximum =
                        self.intrinsic_virtual_generated_size(&child, &sizing_style, false)?;
                    Some(if row { maximum.0 } else { maximum.1 })
                }
                css::IntrinsicSizing::FitContent => {
                    let maximum =
                        self.intrinsic_virtual_generated_size(&child, &sizing_style, false)?;
                    let max_content = if row { maximum.0 } else { maximum.1 };
                    let available_main = if row {
                        Some(width)
                    } else {
                        container_style
                            .height
                            .map(|height| content_height_dimension(container_style, height))
                            .or(self.parent_height)
                    };
                    Some(available_main.map_or(max_content, |available| {
                        available.max(min_content).min(max_content)
                    }))
                }
            }
        } else {
            None
        };
        let main = intrinsic_content_basis.unwrap_or_else(|| {
            item_style.flex_basis.map_or_else(
                || content_basis.unwrap_or(if row { natural.0 } else { natural.1 }),
                |basis| {
                    if row {
                        content_dimension(&item_style, basis) + edges
                    } else {
                        content_height_dimension(&item_style, basis) + edges
                    }
                },
            )
        });
        let mut main = if row {
            constrained_width(&item_style, (main - edges).max(0.0)) + edges
        } else {
            constrained_height(&item_style, (main - edges).max(0.0)) + edges
        };
        let min_main = if row
            && item_style.min_width_auto
            && !item_style.overflow_x.scroll_container()
            || !row && item_style.min_height_auto && !item_style.overflow_y.scroll_container()
        {
            let mut minimum_style = item_style.clone();
            let specified_suggestion = if row {
                minimum_style.width = None;
                item_style
                    .width
                    .map(|value| content_dimension(&item_style, value) + edges)
            } else {
                minimum_style.height = None;
                item_style
                    .height
                    .map(|value| content_height_dimension(&item_style, value) + edges)
            };
            let minimum = self.intrinsic_virtual_generated_size(&child, &minimum_style, true)?;
            let content_minimum = if row { minimum.0 } else { minimum.1 };
            specified_suggestion.map_or(content_minimum, |suggestion| {
                content_minimum.min(suggestion)
            })
        } else {
            0.0
        };
        main = main.max(min_main);
        Ok(FlexItem {
            node: child.origin,
            generated: Some(child),
            style: item_style,
            main,
            min_main,
            source_order,
            cross: if row { natural.1 } else { natural.0 },
            edges,
            cross_edges,
            frozen: false,
            first_command: 0,
            last_command: 0,
            first_hit: 0,
            last_hit: 0,
        })
    }

    fn flex_children(
        &mut self,
        parent: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        width: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        let row = matches!(
            style.flex_direction,
            FlexDirection::Row | FlexDirection::RowReverse
        );
        let reverse = matches!(
            style.flex_direction,
            FlexDirection::RowReverse | FlexDirection::ColumnReverse
        ) ^ (row && style.direction == Direction::Rtl);
        let cross_reverse = style.flex_wrap_reverse ^ (!row && style.direction == Direction::Rtl);
        let definite_height = style
            .height
            .map(|height| content_height_dimension(style, height))
            .filter(|height| height.is_finite() && *height >= 0.0);
        let row_gap = resolved_gap(style.gap, style.row_gap_fraction, definite_height);
        let column_gap = resolved_gap(
            style.column_gap.unwrap_or(0.0),
            style.column_gap_fraction,
            Some(width),
        );
        let main_gap = if row { column_gap } else { row_gap };
        let cross_gap = if row { row_gap } else { column_gap };
        let mut items: Vec<FlexItem> = Vec::new();
        let mut positioned = Vec::new();
        let generated_before =
            self.virtual_generated_child(parent, style, css::PseudoElement::Before, width)?;
        let generated_after =
            self.virtual_generated_child(parent, style, css::PseudoElement::After, width)?;
        let mut source_order = 0;
        if let Some(child) = generated_before {
            items
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            items.push(self.virtual_flex_item(
                child,
                style,
                row,
                width,
                source_order,
                depth + 1,
            )?);
            source_order += 1;
        }
        let mut children = self
            .document
            .composed_children_iter(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            let kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let mut item_style = if matches!(kind, NodeKind::Element { .. }) {
                self.computed_style(node, Some(style))
                    .map_err(LayoutError::Css)?
            } else {
                style.clone()
            };
            let basis = item_style.resolve_flex_basis_percentage(if row {
                Some(width)
            } else {
                self.parent_height
            });
            item_style = item_style.resolve_percentages(width, self.parent_height);
            item_style.flex_basis = basis;
            if !matches!(kind, NodeKind::Text(value) if collapsed_text(value).trim_matches(' ').is_empty())
                && (matches!(kind, NodeKind::Text(_))
                    || matches!(kind, NodeKind::Element { name, namespace: Namespace::Html, .. }
                    if !matches!(name.as_str(), "head" | "style" | "script" | "meta" | "link" | "template")))
                && item_style.display != Display::None
            {
                let (w, h) = self.intrinsic_size(node, &item_style, depth + 1)?;
                if matches!(item_style.position, Position::Absolute | Position::Fixed) {
                    if positioned.len() >= MAX_DISPLAY_COMMANDS {
                        return Err(LayoutError::CommandLimit);
                    }
                    positioned
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    positioned.push((node, item_style, w, h));
                    continue;
                }
                let edges = box_edges(&item_style, row);
                let cross_edges = box_edges(&item_style, !row);
                let content_basis = if item_style.flex_basis_content {
                    let mut content_style = item_style.clone();
                    if row {
                        content_style.width = None;
                    } else {
                        content_style.height = None;
                    }
                    let content_size = self.intrinsic_size(node, &content_style, depth + 1)?;
                    Some(if row { content_size.0 } else { content_size.1 })
                } else {
                    None
                };
                let intrinsic_content_basis = if let Some(kind) = item_style.flex_basis_intrinsic {
                    let mut sizing_style = item_style.clone();
                    if row {
                        sizing_style.width = None;
                    } else {
                        sizing_style.height = None;
                    }
                    sizing_style.flex_basis = None;
                    sizing_style.flex_basis_content = false;
                    sizing_style.flex_basis_intrinsic = None;
                    let minimum = self.intrinsic_size_mode(node, &sizing_style, depth + 1, true)?;
                    let min_content = if row { minimum.0 } else { minimum.1 };
                    match kind {
                        css::IntrinsicSizing::MinContent => Some(min_content),
                        css::IntrinsicSizing::MaxContent => {
                            let maximum =
                                self.intrinsic_size_mode(node, &sizing_style, depth + 1, false)?;
                            Some(if row { maximum.0 } else { maximum.1 })
                        }
                        css::IntrinsicSizing::FitContent => {
                            let maximum =
                                self.intrinsic_size_mode(node, &sizing_style, depth + 1, false)?;
                            let max_content = if row { maximum.0 } else { maximum.1 };
                            let available_main = if row {
                                Some(width)
                            } else {
                                style
                                    .height
                                    .map(|height| content_height_dimension(style, height))
                                    .or(self.parent_height)
                            };
                            Some(available_main.map_or(max_content, |available| {
                                available.max(min_content).min(max_content)
                            }))
                        }
                    }
                } else {
                    None
                };
                let main = intrinsic_content_basis.unwrap_or_else(|| {
                    item_style.flex_basis.map_or_else(
                        || content_basis.unwrap_or(if row { w } else { h }),
                        |basis| {
                            if row {
                                content_dimension(&item_style, basis) + edges
                            } else {
                                content_height_dimension(&item_style, basis) + edges
                            }
                        },
                    )
                });
                let mut main = if row {
                    constrained_width(&item_style, (main - edges).max(0.0)) + edges
                } else {
                    constrained_height(&item_style, (main - edges).max(0.0)) + edges
                };
                let min_main = if row
                    && item_style.min_width_auto
                    && !item_style.overflow_x.scroll_container()
                    || !row
                        && item_style.min_height_auto
                        && !item_style.overflow_y.scroll_container()
                {
                    let mut minimum_style = item_style.clone();
                    let specified_suggestion = if row {
                        minimum_style.width = None;
                        item_style
                            .width
                            .map(|value| content_dimension(&item_style, value) + edges)
                    } else {
                        minimum_style.height = None;
                        item_style
                            .height
                            .map(|value| content_height_dimension(&item_style, value) + edges)
                    };
                    let minimum =
                        self.intrinsic_size_mode(node, &minimum_style, depth + 1, true)?;
                    let content_minimum = if row { minimum.0 } else { minimum.1 };
                    specified_suggestion.map_or(content_minimum, |suggestion| {
                        content_minimum.min(suggestion)
                    })
                } else {
                    0.0
                };
                main = main.max(min_main);
                if items.len() >= MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                items
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                items.push(FlexItem {
                    node,
                    generated: None,
                    style: item_style,
                    main,
                    min_main,
                    source_order,
                    cross: if row { h } else { w },
                    edges,
                    cross_edges,
                    frozen: false,
                    first_command: 0,
                    last_command: 0,
                    first_hit: 0,
                    last_hit: 0,
                });
                source_order += 1;
            }
        }
        if let Some(child) = generated_after {
            if items.len() >= MAX_DISPLAY_COMMANDS {
                return Err(LayoutError::CommandLimit);
            }
            items
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            items.push(self.virtual_flex_item(
                child,
                style,
                row,
                width,
                source_order,
                depth + 1,
            )?);
        }
        if items.is_empty() {
            self.paint_positioned_flex_children(
                positioned, style, x, y, width, row, reverse, depth,
            )?;
            return Ok((0.0, 0.0));
        }
        items.sort_unstable_by_key(|item| (item.style.order, item.source_order));
        let main_limit = if row {
            Some(width)
        } else {
            style
                .height
                .map(|height| content_height_dimension(style, height))
        };
        let mut line_start = 0;
        let mut lines = Vec::new();
        let mut natural_cross = 0.0f32;
        while line_start < items.len() {
            let mut line_end = line_start + 1;
            let mut line_main = items[line_start].main;
            while line_end < items.len() {
                let next = line_main + main_gap + items[line_end].main;
                if style.flex_wrap && main_limit.is_some_and(|limit| next > limit) {
                    break;
                }
                line_main = next;
                line_end += 1;
            }
            let cross = items[line_start..line_end]
                .iter()
                .map(|item| item.cross)
                .fold(0.0f32, f32::max);
            natural_cross += cross;
            lines
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            lines.push((line_start, line_end, cross));
            line_start = line_end;
        }
        natural_cross += cross_gap * lines.len().saturating_sub(1) as f32;
        let cross_limit = if row {
            style
                .height
                .map(|height| content_height_dimension(style, height))
        } else {
            Some(width)
        };
        let cross_free = cross_limit.map_or(0.0, |limit| (limit - natural_cross).max(0.0));
        let line_stretch = if style.align_content.is_none() {
            cross_free / lines.len() as f32
        } else {
            0.0
        };
        let count = lines.len() as f32;
        let (mut cross_offset, line_gap) = match style.align_content {
            Some(JustifyContent::End) => (cross_free, 0.0),
            Some(JustifyContent::Center) => (cross_free * 0.5, 0.0),
            Some(JustifyContent::SpaceBetween) if lines.len() > 1 => {
                (0.0, cross_free / (count - 1.0))
            }
            Some(JustifyContent::SpaceAround) => (cross_free / count * 0.5, cross_free / count),
            Some(JustifyContent::SpaceEvenly) => {
                (cross_free / (count + 1.0), cross_free / (count + 1.0))
            }
            _ => (0.0, 0.0),
        };
        let mut total_main = 0.0f32;
        for (line_start, line_end, line_cross) in lines {
            let items = &mut items[line_start..line_end];
            let line_offset = if cross_reverse {
                cross_limit.unwrap_or(natural_cross) - cross_offset - line_cross - line_stretch
            } else {
                cross_offset
            };
            let (x, y) = if row {
                (x, y + line_offset)
            } else {
                (x + line_offset, y)
            };
            let gaps = main_gap * items.len().saturating_sub(1) as f32;
            let basis: f32 = items.iter().map(|item| item.main).sum();
            let available = if row {
                width
            } else {
                style
                    .height
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(basis + gaps)
            };
            // honey: at most one freezing pass per item; optimize if very large flex lines recur.
            loop {
                let free = available - items.iter().map(|item| item.main).sum::<f32>() - gaps;
                let factor: f32 = items
                    .iter()
                    .filter(|item| !item.frozen)
                    .map(|item| {
                        if free >= 0.0 {
                            item.style.flex_grow
                        } else {
                            item.style.flex_shrink * item.main
                        }
                    })
                    .sum();
                if factor <= 0.0 {
                    break;
                }
                let mut froze = false;
                for item in items.iter_mut() {
                    if item.frozen {
                        continue;
                    }
                    let weight = if free >= 0.0 {
                        item.style.flex_grow
                    } else {
                        item.style.flex_shrink * item.main
                    };
                    let proposed = (item.main
                        + free * weight / if free >= 0.0 { factor.max(1.0) } else { factor })
                    .max(item.edges);
                    item.main = if row {
                        constrained_width(&item.style, proposed - item.edges) + item.edges
                    } else {
                        constrained_height(&item.style, proposed - item.edges) + item.edges
                    }
                    .max(item.min_main);
                    if item.main != proposed {
                        item.frozen = true;
                        froze = true;
                    }
                }
                if !froze {
                    break;
                }
            }
            let remaining =
                (available - gaps - items.iter().map(|item| item.main).sum::<f32>()).max(0.0);
            let (main_start, main_end) = match (row, reverse) {
                (true, false) => (3, 1),
                (true, true) => (1, 3),
                (false, false) => (0, 2),
                (false, true) => (2, 0),
            };
            let auto_count = items
                .iter()
                .map(|item| {
                    usize::from(item.style.margin_auto[main_start])
                        + usize::from(item.style.margin_auto[main_end])
                })
                .sum::<usize>();
            let auto_margin = if auto_count == 0 {
                0.0
            } else {
                remaining / auto_count as f32
            };
            let remaining = if auto_count == 0 { remaining } else { 0.0 };
            let count = items.len() as f32;
            let (start, extra_gap) = match style.justify_content {
                JustifyContent::Start | JustifyContent::Stretch => (0.0, 0.0),
                JustifyContent::End => (remaining, 0.0),
                JustifyContent::Center => (remaining * 0.5, 0.0),
                JustifyContent::SpaceBetween if items.len() > 1 => (0.0, remaining / (count - 1.0)),
                JustifyContent::SpaceAround => (remaining / count * 0.5, remaining / count),
                JustifyContent::SpaceEvenly => {
                    (remaining / (count + 1.0), remaining / (count + 1.0))
                }
                _ => (0.0, 0.0),
            };
            let cross_extent = if style.flex_wrap {
                line_cross + line_stretch
            } else if row {
                style
                    .height
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(items.iter().map(|item| item.cross).fold(0.0f32, f32::max))
            } else {
                width
            };
            let mut cursor = start;
            let mut actual_cross = 0.0f32;
            for item in items.iter_mut() {
                cursor += if item.style.margin_auto[main_start] {
                    auto_margin
                } else {
                    0.0
                };
                let main_pos = if reverse {
                    available - cursor - item.main
                } else {
                    cursor
                };
                let mut computed = item.style.clone();
                if row {
                    computed.width = Some(specified_dimension(
                        &computed,
                        (item.main - item.edges).max(0.0),
                    ));
                } else {
                    computed.height = Some(specified_height(
                        &computed,
                        (item.main - item.edges).max(0.0),
                    ));
                }
                if computed.display == Display::Inline {
                    computed.display = Display::Block;
                }
                let cross_auto = if row {
                    computed.margin_auto[0] || computed.margin_auto[2]
                } else {
                    computed.margin_auto[1] || computed.margin_auto[3]
                };
                if !cross_auto
                    && computed.align_self.unwrap_or(style.align_items) == AlignItems::Stretch
                {
                    if row
                        && (style.height.is_some() || style.flex_wrap)
                        && computed.height.is_none()
                        && computed.height_intrinsic.is_none()
                    {
                        computed.height = Some(specified_height(
                            &computed,
                            (cross_extent - item.cross_edges).max(0.0),
                        ));
                    }
                    if !row && computed.width.is_none() {
                        computed.width = Some(specified_dimension(
                            &computed,
                            (cross_extent - item.cross_edges).max(0.0),
                        ));
                    }
                }
                item.first_command = self.commands.len();
                item.first_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                let (child_x, child_y) = if row {
                    (x + main_pos, y)
                } else {
                    (x, y + main_pos)
                };
                let cross = if let Some(generated) = item.generated.as_ref() {
                    let mut generated_style = computed.clone();
                    generated_style.display = Display::Block;
                    let height = self.box_for_virtual_generated(
                        generated,
                        style,
                        &generated_style,
                        child_x,
                        child_y,
                        if row { item.main } else { width },
                        &[],
                        depth + 1,
                    )?;
                    if row {
                        height
                    } else {
                        generated_style
                            .width
                            .map(|value| {
                                constrained_width(
                                    &generated_style,
                                    content_dimension(&generated_style, value),
                                )
                            })
                            .unwrap_or_else(|| {
                                constrained_width(&generated_style, width - item.cross_edges)
                            })
                            .max(0.0)
                            + item.cross_edges
                    }
                } else if let NodeKind::Text(value) = self
                    .document
                    .kind(item.node)
                    .map_err(|_| LayoutError::InvalidTree)?
                {
                    let (mut text_y, mut advance, mut height) = (child_y, 0.0, 0.0);
                    let mut first = self.line_start();
                    let mut trailing = 0.0;
                    self.text_flow(
                        value,
                        style,
                        child_x,
                        if row { item.main } else { width },
                        &mut text_y,
                        &mut advance,
                        &mut height,
                        &[],
                        &mut first,
                        &mut trailing,
                    )?;
                    self.align_line(
                        first,
                        child_x,
                        if row { item.main } else { width },
                        advance - trailing,
                        style,
                        true,
                    );
                    if row {
                        text_y - child_y + height
                    } else {
                        advance
                    }
                } else {
                    let height = self.box_for(
                        item.node,
                        style,
                        Some(computed.clone()),
                        child_x,
                        child_y,
                        if row { item.main } else { width },
                        depth + 1,
                    )?;
                    if row {
                        height
                    } else {
                        computed
                            .width
                            .map(|value| {
                                constrained_width(&computed, content_dimension(&computed, value))
                            })
                            .unwrap_or_else(|| {
                                constrained_width(&computed, width - item.cross_edges)
                            })
                            .max(0.0)
                            + item.cross_edges
                    }
                };
                item.cross = cross;
                actual_cross = actual_cross.max(cross);
                item.last_command = self.commands.len();
                item.last_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                cursor += item.main
                    + main_gap
                    + extra_gap
                    + if item.style.margin_auto[main_end] {
                        auto_margin
                    } else {
                        0.0
                    };
            }
            let align_extent = if style.flex_wrap {
                actual_cross.max(cross_extent)
            } else if row {
                style
                    .height
                    .filter(|_| !style.flex_wrap)
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(actual_cross)
            } else {
                width
            };
            let baseline = row.then(|| {
                items
                    .iter()
                    .filter(|item| {
                        item.style.align_self.unwrap_or(style.align_items) == AlignItems::Baseline
                    })
                    .map(|item| (item.cross - item.style.margin_sides[2]).max(0.0))
                    .fold(0.0f32, f32::max)
            });
            let mut command_bytes = self.command_bytes;
            for item in items.iter() {
                let align = item.style.align_self.unwrap_or(style.align_items);
                if row
                    && align == AlignItems::Stretch
                    && item.style.height.is_none()
                    && item.style.height_intrinsic.is_none()
                    && !item.style.margin_auto[0]
                    && !item.style.margin_auto[2]
                {
                    let root_height =
                        (align_extent - item.style.margin_sides[0] - item.style.margin_sides[2])
                            .max(0.0);
                    let paints = if has_background(&item.style) {
                        background_paints(&item.style)
                    } else {
                        0
                    } + usize::from(
                        item.style.border_solid
                            && item.style.border_width > 0.0
                            && item.style.border_color.a != 0,
                    );
                    let mut layers = background_layers(&item.style).map(|layers| {
                        layers
                            .into_iter()
                            .enumerate()
                            .rev()
                            .filter(|(_, layer)| !matches!(layer.image, BackgroundImage::None))
                            .collect::<Vec<_>>()
                            .into_iter()
                    });
                    for command in self.commands[item.first_command..item.last_command]
                        .iter_mut()
                        .take(
                            paints
                                + usize::from(item.style.opacity < 1.0)
                                + usize::from(item.style.transforms.is_some()),
                        )
                    {
                        match command {
                            Command::MaskedBackground(mask) => {
                                let layer = if matches!(*mask.paint, Command::FillBackground(_)) {
                                    layers.as_mut().and_then(|layers| layers.next())
                                } else {
                                    None
                                };
                                stretch_background_mask(
                                    command,
                                    &item.style,
                                    root_height,
                                    layer,
                                    self.images,
                                    self.viewport,
                                    &mut command_bytes,
                                )?;
                            }
                            Command::StrokeBoxBorder(border) => {
                                border.rect.height = root_height;
                                border.corners =
                                    clipped_corners(border.rect, border.rect, &item.style)
                                        .map(|r| *r);
                            }
                            Command::FillRect { rect, .. }
                            | Command::StrokeBorder { rect, .. }
                            | Command::StrokePatternBorder { rect, .. } => {
                                rect.height = root_height
                            }
                            Command::FillRoundedRect { rect, .. } => {
                                let inset = background_bleed_inset(&item.style);
                                let outer = Rect {
                                    x: rect.x - inset,
                                    y: rect.y - inset,
                                    width: rect.width + 2.0 * inset,
                                    height: root_height,
                                };
                                replace_command_bytes(
                                    command,
                                    &mut command_bytes,
                                    background_color(outer, &item.style),
                                )?;
                            }
                            Command::FillBackground(fill) => {
                                let rect = &fill.rect;
                                if let Some((layer_index, layer)) =
                                    layers.as_mut().and_then(|layers| layers.next())
                                {
                                    let border = used_border(&item.style);
                                    let outer = Rect {
                                        x: rect.x - border,
                                        y: rect.y - border,
                                        width: rect.width + 2.0 * border,
                                        height: root_height,
                                    };
                                    let paint = background_paint(&layer, self.images);
                                    replace_command_bytes(
                                        command,
                                        &mut command_bytes,
                                        background_command(
                                            outer,
                                            &item.style,
                                            &layer,
                                            layer_index,
                                            &paint,
                                            self.viewport,
                                            None,
                                        ),
                                    )?;
                                }
                            }
                            Command::BoxShadow {
                                rect,
                                corners,
                                shadow,
                                ..
                            } => {
                                stretch_shadow(
                                    rect,
                                    corners,
                                    shadow.inset,
                                    root_height,
                                    &item.style,
                                );
                            }
                            _ => {}
                        }
                    }
                    if let Some(geometry) = self.geometry.as_mut() {
                        if let Some(hit) = geometry.hits.get_mut(item.first_hit) {
                            if hit.node == item.node {
                                hit.rect.height = root_height;
                            }
                        }
                    }
                }
                self.refresh_transform(
                    &item.style,
                    item.first_command..item.last_command,
                    item.first_hit..item.last_hit,
                )?;
                let free = (align_extent - item.cross).max(0.0);
                let (cross_start, cross_end) = if row { (0, 2) } else { (3, 1) };
                let offset = if item.style.margin_auto[cross_start] {
                    if item.style.margin_auto[cross_end] {
                        free * 0.5
                    } else {
                        free
                    }
                } else if item.style.margin_auto[cross_end] {
                    0.0
                } else {
                    match align {
                        AlignItems::End => free,
                        AlignItems::Center => free * 0.5,
                        AlignItems::Baseline if row => {
                            baseline.unwrap_or(0.0)
                                - (item.cross - item.style.margin_sides[2]).max(0.0)
                        }
                        _ => 0.0,
                    }
                };
                let offset = if cross_reverse
                    && align != AlignItems::Baseline
                    && !item.style.margin_auto[cross_start]
                    && !item.style.margin_auto[cross_end]
                {
                    free - offset
                } else {
                    offset
                };
                let (dx, dy) = if row { (0.0, offset) } else { (offset, 0.0) };
                for command in &mut self.commands[item.first_command..item.last_command] {
                    move_command(command, dx, dy);
                }
                if let Some(geometry) = self.geometry.as_mut() {
                    geometry.move_hits(item.first_hit..item.last_hit, dx, dy);
                }
            }
            self.command_bytes = command_bytes;
            cross_offset += align_extent + cross_gap + line_gap;
            total_main = total_main.max(available);
        }
        cross_offset = (cross_offset - cross_gap - line_gap).max(0.0);
        if style.flex_wrap && cross_limit.is_some() {
            cross_offset = cross_limit.unwrap();
        }
        self.paint_positioned_flex_children(positioned, style, x, y, width, row, reverse, depth)?;
        Ok(if row {
            (total_main, cross_offset)
        } else {
            (cross_offset, total_main)
        })
    }
    fn cull_right(&self) -> f32 {
        self.cull.x + self.cull.width
    }

    fn cull_bottom(&self) -> f32 {
        self.cull.y + self.cull.height
    }

    fn line_height(&self, style: &Style) -> f32 {
        match style.line_height {
            LineHeight::Normal => self.text.line_height_styled(style.font_size, &style.font),
            LineHeight::Number(value) => value * style.font_size,
            LineHeight::Pixels(value) => value,
        }
    }

    fn baseline(&self, style: &Style) -> f32 {
        self.text.ascent_styled(style.font_size, &style.font)
            + (self.line_height(style) - self.text.line_height_styled(style.font_size, &style.font))
                * 0.5
    }

    fn paragraph_inline_style(style: &Style, parent: &Style) -> bool {
        style.display == Display::Inline
            && style.position == Position::Static
            && style.float == Float::None
            && style.clear == Clear::None
            && style.opacity == 1.0
            && style.transforms.is_none()
            && !style.overflow_clip
            && !style.contain_paint
            && !style.contain_layout
            && style.margin_sides == [0.0; 4]
            && style.padding_sides == [0.0; 4]
            && border_widths(style) == [0.0; 4]
            && style.white_space == parent.white_space
            && style.writing_mode == parent.writing_mode
            && style.direction == parent.direction
    }

    fn virtual_generated_child(
        &self,
        origin: NodeId,
        origin_style: &Style,
        pseudo: css::PseudoElement,
        available: f32,
    ) -> Result<Option<VirtualGeneratedChild>, LayoutError> {
        let Some(generated) = self
            .rules
            .compute_pseudo(self.document, origin, origin_style, pseudo, Some(self.text))
            .map_err(LayoutError::Css)?
        else {
            return Ok(None);
        };
        if generated.style.display == Display::None {
            return Ok(None);
        }
        let css::GeneratedContent::Items(items) = generated.content else {
            return Ok(None);
        };
        let mut child = VirtualGeneratedChild {
            origin,
            pseudo,
            style: generated
                .style
                .resolve_percentages(available, self.parent_height),
            unresolved_style: generated.style,
            items,
            text: Arc::from(""),
        };
        let text = self.resolve_generated_text(&child)?;
        child.text = Arc::from(text);
        Ok(Some(child))
    }

    fn generated_attribute(
        &self,
        origin: NodeId,
        requested: &str,
    ) -> Result<Option<String>, LayoutError> {
        let NodeKind::Element {
            attributes,
            namespace,
            ..
        } = self
            .document
            .kind(origin)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Err(LayoutError::InvalidTree);
        };
        let html_attribute_names =
            self.document.is_html_document() && *namespace == Namespace::Html;
        if html_attribute_names {
            for (name, _) in attributes {
                if name.as_str().eq_ignore_ascii_case(requested) {
                    if let Some(value) = self
                        .document
                        .get_attribute_ns(origin, None, name.as_str())
                        .map_err(|_| LayoutError::InvalidTree)?
                    {
                        return Ok(Some(value));
                    }
                }
            }
            Ok(None)
        } else {
            self.document
                .get_attribute_ns(origin, None, requested)
                .map_err(|_| LayoutError::InvalidTree)
        }
    }

    fn resolve_generated_text(&self, child: &VirtualGeneratedChild) -> Result<String, LayoutError> {
        let mut output = String::new();
        for item in child.items.iter() {
            let value = match item {
                css::GeneratedContentItem::String(value) => Some(Cow::Borrowed(value.as_ref())),
                css::GeneratedContentItem::Attribute { name, fallback } => self
                    .generated_attribute(child.origin, name)?
                    .map(Cow::Owned)
                    .or_else(|| fallback.as_deref().map(Cow::Borrowed)),
                // Alternative text describes the generated image for
                // nonvisual consumers; it is not part of its painted output.
                css::GeneratedContentItem::AlternativeText(_) => None,
                css::GeneratedContentItem::Url(_)
                | css::GeneratedContentItem::Counter { .. }
                | css::GeneratedContentItem::Counters { .. }
                | css::GeneratedContentItem::OpenQuote
                | css::GeneratedContentItem::CloseQuote
                | css::GeneratedContentItem::NoOpenQuote
                | css::GeneratedContentItem::NoCloseQuote
                | css::GeneratedContentItem::UnsupportedFunction { .. } => {
                    return Err(LayoutError::UnsupportedGeneratedContent);
                }
            };
            let Some(value) = value else {
                continue;
            };
            let end = output
                .len()
                .checked_add(value.len())
                .filter(|end| *end <= css::MAX_GENERATED_CONTENT_BYTES)
                .ok_or(LayoutError::CommandLimit)?;
            output
                .try_reserve(end - output.len())
                .map_err(|_| LayoutError::CommandLimit)?;
            output.push_str(value.as_ref());
        }
        Ok(output)
    }

    fn generated_text<'a>(&self, child: &'a VirtualGeneratedChild) -> &'a str {
        child.text.as_ref()
    }

    fn append_virtual_generated_child(
        &self,
        child: &VirtualGeneratedChild,
        origin_style: &Style,
        paragraph: &mut InlineParagraph,
        frame_path: &mut Vec<usize>,
    ) -> Result<(), LayoutError> {
        if child.style.display != Display::Inline
            || !Self::paragraph_inline_style(&child.style, origin_style)
        {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }
        self.append_virtual_generated_frame(child, &child.style, paragraph, frame_path)
    }

    fn append_virtual_generated_frame(
        &self,
        child: &VirtualGeneratedChild,
        style: &Style,
        paragraph: &mut InlineParagraph,
        frame_path: &mut Vec<usize>,
    ) -> Result<(), LayoutError> {
        if paragraph.frames.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        let text = self.generated_text(child);
        paragraph
            .frames
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        let frame = paragraph.frames.len();
        paragraph.frames.push(InlineFrame {
            node: child.origin,
            style: style.clone(),
            virtual_pseudo: Some(child.pseudo),
            hit: None,
            bounds: None,
        });
        frame_path
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        frame_path.push(frame);
        Self::paragraph_append_text(paragraph, text, child.origin, style, frame_path)?;
        frame_path.pop();
        Ok(())
    }

    fn intrinsic_virtual_generated_size(
        &self,
        child: &VirtualGeneratedChild,
        style: &Style,
        minimum: bool,
    ) -> Result<(f32, f32), LayoutError> {
        let text = self.generated_text(child);
        let text = collapsed_text(text);
        let content_width = if let Some(width) = style.width {
            content_dimension(style, width)
        } else if minimum && !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre) {
            let mut width = 0.0f32;
            for word in text.split_ascii_whitespace() {
                width = width.max(
                    self.text
                        .measure_styled(word, style.font_size, &style.font)
                        .map_err(|_| LayoutError::Text)?,
                );
            }
            width
        } else {
            self.text
                .measure_styled(text.trim_matches(' '), style.font_size, &style.font)
                .map_err(|_| LayoutError::Text)?
        };
        let content_height = style
            .height
            .map(|height| content_height_dimension(style, height))
            .unwrap_or_else(|| {
                if text.is_empty() {
                    0.0
                } else {
                    self.line_height(style)
                }
            });
        Ok((
            constrained_width(style, content_width) + box_edges(style, true),
            constrained_height(style, content_height) + box_edges(style, false),
        ))
    }

    /// Paint a generated child as a standalone block box. Its text still uses
    /// the normal inline shaper, while the virtual hit owner stays attached to
    /// the originating element and is marked out of DOM geometry queries.
    fn box_for_virtual_generated(
        &mut self,
        child: &VirtualGeneratedChild,
        parent_style: &Style,
        style: &Style,
        x: f32,
        y: f32,
        available: f32,
        floats: &[(Rect, Float)],
        depth: usize,
    ) -> Result<f32, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        if style.display != Display::Block
            || style.position != Position::Static
            || style.float != Float::None
            || style.clear != Clear::None
            || style.opacity != 1.0
            || style.transforms.is_some()
            || style.overflow_clip
            || overflow_scroll_container(style)
            || overflow_clips_x(style)
            || overflow_clips_y(style)
            || style.contain_paint
            || style.contain_layout
        {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }

        let [border_top, border_right, border_bottom, border_left] = border_widths(style);
        let [margin_top, margin_right, margin_bottom, margin_left] = style.margin_sides;
        let [padding_top, padding_right, padding_bottom, padding_left] = style.padding_sides;
        let left_auto = style.margin_auto[3];
        let right_auto = style.margin_auto[1];
        let left = if left_auto { 0.0 } else { margin_left };
        let right = if right_auto { 0.0 } else { margin_right };
        let content_width = constrained_width(
            style,
            style
                .width
                .map(|width| content_dimension(style, width))
                .unwrap_or(
                    (available
                        - left
                        - right
                        - padding_left
                        - padding_right
                        - border_left
                        - border_right)
                        .max(0.0),
                ),
        );
        let free = available
            - content_width
            - padding_left
            - padding_right
            - border_left
            - border_right
            - left
            - right;
        let (used_left, used_right) = if free >= 0.0 && (left_auto || right_auto) {
            match (left_auto, right_auto) {
                (true, true) => (free * 0.5, free * 0.5),
                (true, false) => (free, right),
                _ => (left, free),
            }
        } else if parent_style.direction == Direction::Rtl {
            (left + free, right)
        } else {
            (left, right)
        };
        let outer_x = x + used_left;
        let outer_y = y + margin_top;
        let outer_width = content_width + padding_left + padding_right + border_left + border_right;
        let background_index = if style.visibility_visible
            && has_background(style)
            && (self.transform_depth > 0
                || (outer_y < self.cull_bottom() && outer_x < self.cull_right()))
        {
            let index = self.commands.len() + background_paints(style) - 1;
            self.push_background(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: outer_width,
                    height: 0.0,
                },
                style,
            )?;
            Some(index)
        } else {
            None
        };
        let border_index = if style.visibility_visible
            && !has_nonuniform_border(style)
            && border_widths(style).into_iter().fold(0.0f32, f32::max) > 0.0
            && style.border_color.a > 0
            && (self.transform_depth > 0
                || (outer_y < self.cull_bottom() && outer_x < self.cull_right()))
        {
            let index = self.commands.len();
            self.push_command(border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: outer_width,
                    height: 0.0,
                },
                style,
            ))?;
            Some(index)
        } else {
            None
        };
        let hit = self.begin_virtual_hit(child.origin)?;
        let saved_decorations = self.decorations.len();
        self.begin_decoration(style)?;
        let mut paragraph = InlineParagraph::default();
        let mut frame_path = Vec::new();
        self.append_virtual_generated_frame(child, style, &mut paragraph, &mut frame_path)?;
        let content_x = outer_x + border_left + padding_left;
        let content_y = outer_y + border_top + padding_top;
        let mut cursor = content_y;
        let (mut advance, mut line_height, mut trailing) = (0.0, 0.0, 0.0);
        let mut line = self.line_start();
        self.flow_inline_paragraph(
            &mut paragraph,
            style,
            content_x,
            content_width,
            &mut cursor,
            &mut advance,
            &mut line_height,
            floats,
            &mut line,
            &mut trailing,
            depth + 1,
        )?;
        self.align_line(
            line,
            content_x,
            content_width,
            advance - trailing,
            style,
            true,
        );
        cursor += line_height;
        self.decorations.truncate(saved_decorations);

        let intrinsic_height = (cursor - content_y).max(0.0);
        let content_height = style
            .height
            .map(|height| content_height_dimension(style, height))
            .unwrap_or(intrinsic_height)
            .min(style.max_height.unwrap_or(f32::INFINITY))
            .max(style.min_height)
            .max(0.0);
        let outer_height =
            content_height + padding_top + padding_bottom + border_top + border_bottom;
        let outer_rect = Rect {
            x: outer_x,
            y: outer_y,
            width: outer_width,
            height: outer_height,
        };
        self.finish_hit(hit, outer_rect);
        if let Some(index) = background_index {
            self.finish_background(
                index,
                outer_rect,
                style,
                Some(Rect {
                    x: outer_x + border_left,
                    y: outer_y + border_top,
                    width: content_width + padding_left + padding_right,
                    height: content_height + padding_top + padding_bottom,
                }),
            )?;
        }
        if let Some(index) = border_index {
            self.replace_command(index, border(outer_rect, style))?;
        } else if style.visibility_visible && has_nonuniform_border(style) {
            for command in side_border_commands(outer_rect, style) {
                self.push_command(command)?;
            }
        }
        Ok(outer_height + margin_top + margin_bottom)
    }

    fn flow_virtual_generated_block_child(
        &mut self,
        child: &VirtualGeneratedChild,
        parent_style: &Style,
        left: f32,
        width: f32,
        cursor: &mut f32,
        inline_width: &mut f32,
        inline_height: &mut f32,
        floats: &[(Rect, Float)],
        line: &mut LineStart,
        trailing: &mut f32,
        previous_margin: &mut f32,
        block_children: &mut Vec<BlockChild>,
        non_block_since: &mut bool,
        last_was_block: &mut bool,
        content_extent_width: &mut f32,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if child.style.display != Display::Block || !floats.is_empty() {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }
        self.align_line(
            *line,
            left,
            width,
            *inline_width - *trailing,
            parent_style,
            true,
        );
        *cursor += *inline_height;
        *inline_width = 0.0;
        *inline_height = 0.0;
        *trailing = 0.0;
        *line = self.line_start();

        let margin_top = child.style.margin_sides[0];
        let margin_bottom = child.style.margin_sides[2];
        let preceding_margin = *previous_margin;
        let collapsed =
            preceding_margin.max(margin_top).max(0.0) + preceding_margin.min(margin_top).min(0.0);
        if preceding_margin != 0.0 {
            *cursor += collapsed - preceding_margin - margin_top;
        }
        let child_y = *cursor;
        let first_command = self.commands.len();
        let first_hit = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        let advance = self.box_for_virtual_generated(
            child,
            parent_style,
            &child.style,
            left,
            child_y,
            width,
            floats,
            depth + 1,
        )?;
        if block_children.len() >= 4096 {
            return Err(LayoutError::CommandLimit);
        }
        block_children
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        block_children.push(BlockChild {
            first_cmd: first_command,
            first_hit,
            border_top: child_y + margin_top,
            margin_top,
            margin_bottom,
            through: None,
            advance,
            break_before: *non_block_since,
        });
        let horizontal_extent = child
            .style
            .width
            .map(|width| content_dimension(&child.style, width))
            .unwrap_or(
                (width - child.style.margin_sides[1] - child.style.margin_sides[3]).max(0.0),
            )
            + child.style.padding_sides[1]
            + child.style.padding_sides[3]
            + border_widths(&child.style)[1]
            + border_widths(&child.style)[3]
            + child.style.margin_sides[1]
            + child.style.margin_sides[3];
        *content_extent_width = (*content_extent_width).max(horizontal_extent);
        *cursor += advance;
        *previous_margin = margin_bottom;
        *non_block_since = false;
        *last_was_block = true;
        *line = self.line_start();
        Ok(())
    }

    fn flow_virtual_generated_child(
        &mut self,
        child: &VirtualGeneratedChild,
        origin_style: &Style,
        left: f32,
        width: f32,
        cursor: &mut f32,
        advance: &mut f32,
        line_height: &mut f32,
        floats: &[(Rect, Float)],
        line: &mut LineStart,
        trailing: &mut f32,
        depth: usize,
    ) -> Result<bool, LayoutError> {
        if child.style.display != Display::Inline
            || !Self::paragraph_inline_style(&child.style, origin_style)
        {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }
        let text = self.generated_text(child);
        let mut paragraph = InlineParagraph {
            has_preceding_content: *advance > 0.0,
            ..InlineParagraph::default()
        };
        paragraph
            .frames
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        paragraph.frames.push(InlineFrame {
            node: child.origin,
            style: child.style.clone(),
            virtual_pseudo: Some(child.pseudo),
            hit: None,
            bounds: None,
        });
        let mut frame_path = Vec::new();
        frame_path
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        frame_path.push(0);
        Self::paragraph_append_text(
            &mut paragraph,
            text,
            child.origin,
            &child.style,
            &frame_path,
        )?;
        self.flow_inline_paragraph(
            &mut paragraph,
            origin_style,
            left,
            width,
            cursor,
            advance,
            line_height,
            floats,
            line,
            trailing,
            depth,
        )?;
        // Even empty generated content creates a virtual inline child and a
        // line box when its style has nonzero line-height.
        Ok(true)
    }

    fn paragraph_inline_eligible(
        &self,
        node: NodeId,
        parent_style: &Style,
        computed: Option<&Style>,
        available: f32,
        depth: usize,
    ) -> Result<bool, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        match self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        {
            NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {
                return Ok(true);
            }
            NodeKind::Element {
                name,
                namespace: Namespace::Svg,
                ..
            } if crate::svg::local_name(name) == "svg" => {}
            NodeKind::Element { namespace, .. } if *namespace != Namespace::Html => {
                return Ok(true);
            }
            NodeKind::Element { name, .. }
                if matches!(
                    crate::svg::local_name(name),
                    "head" | "style" | "script" | "meta" | "link" | "template" | "title" | "base"
                ) =>
            {
                return Ok(true);
            }
            NodeKind::Element { .. } => {}
            _ => return Ok(false),
        }
        let mut style = match computed {
            Some(style) => style.clone(),
            None => self
                .computed_style(node, Some(parent_style))
                .map_err(LayoutError::Css)?,
        };
        style = style.resolve_percentages(available, self.parent_height);
        if style.display == Display::None {
            return Ok(true);
        }
        let NodeKind::Element {
            name, namespace, ..
        } = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Ok(false);
        };
        let tag = crate::svg::local_name(name);
        let svg_root = *namespace == Namespace::Svg && tag == "svg";
        if tag == "br" {
            for pseudo in [css::PseudoElement::Before, css::PseudoElement::After] {
                if let Some(generated) =
                    self.virtual_generated_child(node, &style, pseudo, available)?
                {
                    if generated.style.display != Display::Inline
                        || !Self::paragraph_inline_style(&generated.style, &style)
                    {
                        return Err(LayoutError::UnsupportedGeneratedContent);
                    }
                }
            }
            return Ok(true);
        }
        if (svg_root || tag == "img" || tag == "canvas") && style.display == Display::Inline
            || style.display == Display::InlineBlock
        {
            return Ok(style.position == Position::Static
                && style.float == Float::None
                && style.opacity == 1.0
                && style.transforms.is_none()
                && !style.overflow_clip
                && !style.contain_paint
                && !style.contain_layout);
        }
        if !Self::paragraph_inline_style(&style, parent_style) {
            return Ok(false);
        }
        for pseudo in [css::PseudoElement::Before, css::PseudoElement::After] {
            if let Some(generated) =
                self.virtual_generated_child(node, &style, pseudo, available)?
            {
                if generated.style.display != Display::Inline
                    || !Self::paragraph_inline_style(&generated.style, &style)
                {
                    return Err(LayoutError::UnsupportedGeneratedContent);
                }
            }
        }
        let mut children = self
            .document
            .composed_children_iter(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(child) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            let child_style = if matches!(
                self.document
                    .kind(child)
                    .map_err(|_| LayoutError::InvalidTree)?,
                NodeKind::Element { .. }
            ) {
                Some(
                    self.computed_style(child, Some(&style))
                        .map_err(LayoutError::Css)?
                        .resolve_percentages(available, self.parent_height),
                )
            } else {
                None
            };
            if !self.paragraph_inline_eligible(
                child,
                &style,
                child_style.as_ref(),
                available,
                depth + 1,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn paragraph_push_span(
        paragraph: &mut InlineParagraph,
        text: &str,
        style_node: NodeId,
        style: &Style,
        frames: &[usize],
    ) -> Result<(), LayoutError> {
        if text.is_empty() {
            return Ok(());
        }
        let end = paragraph
            .text
            .len()
            .checked_add(text.len())
            .filter(|end| *end <= lumen_common::bidi::MAX_TEXT_BYTES)
            .ok_or(LayoutError::Text)?;
        paragraph
            .text
            .try_reserve(text.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        let start = paragraph.text.len();
        if let Some(previous) = paragraph.spans.last_mut() {
            if previous.range.end == start
                && previous.style_node == style_node
                && previous.frames.as_slice() == frames
            {
                paragraph.text.push_str(text);
                previous.range.end = end;
                return Ok(());
            }
        }
        if paragraph.spans.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        let mut path = Vec::new();
        path.try_reserve(frames.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        path.extend_from_slice(frames);
        paragraph
            .spans
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        paragraph.text.push_str(text);
        paragraph.spans.push(InlineTextSpan {
            range: start..end,
            style_node,
            style: style.clone(),
            frames: path,
        });
        Ok(())
    }

    fn paragraph_append_text(
        paragraph: &mut InlineParagraph,
        value: &str,
        style_node: NodeId,
        style: &Style,
        frames: &[usize],
    ) -> Result<(), LayoutError> {
        let mut output = String::new();
        let mut local_breaks = Vec::new();
        let collapse = matches!(
            style.white_space,
            WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine
        );
        let preserve_newline = matches!(
            style.white_space,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces | WhiteSpace::PreLine
        );
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            let crlf = character == '\r' && chars.peek() == Some(&'\n');
            if crlf {
                chars.next();
            }
            let newline = matches!(character, '\n' | '\r');
            if newline && preserve_newline {
                output.push('\u{200b}');
                local_breaks
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                local_breaks.push(output.len());
                paragraph.collapse_space = false;
            } else if collapse && matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0c') {
                if !paragraph.collapse_space {
                    let previous_is_break =
                        paragraph.text.ends_with('\u{200b}') || output.ends_with('\u{200b}');
                    if !paragraph.text.is_empty()
                        || !output.is_empty()
                        || paragraph.has_preceding_content
                    {
                        if !previous_is_break {
                            output.push(' ');
                        }
                    }
                }
                paragraph.collapse_space = true;
            } else {
                output.push(character);
                paragraph.collapse_space = false;
            }
        }
        if paragraph.text.len().saturating_add(output.len()) > lumen_common::bidi::MAX_TEXT_BYTES {
            return Err(LayoutError::Text);
        }
        let start = paragraph.text.len();
        Self::paragraph_push_span(paragraph, &output, style_node, style, frames)?;
        if !local_breaks.is_empty() {
            if paragraph
                .hard_breaks
                .len()
                .saturating_add(local_breaks.len())
                > MAX_DISPLAY_COMMANDS
            {
                return Err(LayoutError::CommandLimit);
            }
            paragraph
                .hard_breaks
                .try_reserve(local_breaks.len())
                .map_err(|_| LayoutError::CommandLimit)?;
            paragraph
                .hard_breaks
                .extend(local_breaks.into_iter().map(|offset| start + offset));
        }
        Ok(())
    }

    fn paragraph_append_break(
        paragraph: &mut InlineParagraph,
        style_node: NodeId,
        style: &Style,
        frames: &[usize],
    ) -> Result<(), LayoutError> {
        if paragraph.hard_breaks.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        Self::paragraph_push_span(paragraph, "\u{200b}", style_node, style, frames)?;
        paragraph
            .hard_breaks
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        paragraph.hard_breaks.push(paragraph.text.len());
        paragraph.collapse_space = false;
        Ok(())
    }

    fn collect_inline_paragraph_node(
        &mut self,
        node: NodeId,
        parent_node: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        available: f32,
        paragraph: &mut InlineParagraph,
        frame_path: &mut Vec<usize>,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        match kind {
            NodeKind::Text(value) => {
                return Self::paragraph_append_text(
                    paragraph,
                    value,
                    parent_node,
                    parent_style,
                    frame_path,
                );
            }
            NodeKind::Element {
                name,
                namespace: Namespace::Svg,
                ..
            } if crate::svg::local_name(name) == "svg" => {}
            NodeKind::Element { namespace, .. } if *namespace != Namespace::Html => {
                return Ok(());
            }
            NodeKind::Element { name, .. }
                if matches!(
                    crate::svg::local_name(name),
                    "head" | "style" | "script" | "meta" | "link" | "template" | "title" | "base"
                ) =>
            {
                return Ok(());
            }
            NodeKind::Element { .. } => {}
            _ => return Ok(()),
        }
        let mut style = match computed {
            Some(style) => style,
            None => self
                .computed_style(node, Some(parent_style))
                .map_err(LayoutError::Css)?,
        };
        style = style.resolve_percentages(available, self.parent_height);
        if style.display == Display::None {
            return Ok(());
        }
        let NodeKind::Element {
            name, namespace, ..
        } = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Ok(());
        };
        let tag = crate::svg::local_name(name);
        let svg_root = *namespace == Namespace::Svg && tag == "svg";
        if tag == "br" {
            if let Some(before) =
                self.virtual_generated_child(node, &style, css::PseudoElement::Before, available)?
            {
                self.append_virtual_generated_child(&before, &style, paragraph, frame_path)?;
            }
            Self::paragraph_append_break(paragraph, parent_node, parent_style, frame_path)?;
            if let Some(after) =
                self.virtual_generated_child(node, &style, css::PseudoElement::After, available)?
            {
                self.append_virtual_generated_child(&after, &style, paragraph, frame_path)?;
            }
            return Ok(());
        }
        if svg_root || tag == "img" || tag == "canvas" || style.display == Display::InlineBlock {
            if paragraph.atoms.len() >= MAX_DISPLAY_COMMANDS {
                return Err(LayoutError::CommandLimit);
            }
            let (width, height) = self.intrinsic_size(node, &style, depth + 1)?;
            let start = paragraph.text.len();
            let end = start
                .checked_add('\u{fffc}'.len_utf8())
                .ok_or(LayoutError::Text)?;
            if end > lumen_common::bidi::MAX_TEXT_BYTES {
                return Err(LayoutError::Text);
            }
            paragraph
                .text
                .try_reserve('\u{fffc}'.len_utf8())
                .map_err(|_| LayoutError::CommandLimit)?;
            paragraph.text.push('\u{fffc}');
            let mut frames = Vec::new();
            frames
                .try_reserve(frame_path.len())
                .map_err(|_| LayoutError::CommandLimit)?;
            frames.extend_from_slice(frame_path);
            paragraph
                .atoms
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            paragraph.atoms.push(InlineAtom {
                range: start..paragraph.text.len(),
                node,
                parent_style: parent_style.clone(),
                style,
                frames,
                width,
                height,
            });
            paragraph.collapse_space = false;
            return Ok(());
        }
        if !Self::paragraph_inline_style(&style, parent_style) {
            return Err(LayoutError::InvalidTree);
        }
        if paragraph.frames.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        paragraph
            .frames
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        let frame_index = paragraph.frames.len();
        paragraph.frames.push(InlineFrame {
            node,
            style: style.clone(),
            virtual_pseudo: None,
            hit: None,
            bounds: None,
        });
        frame_path
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        frame_path.push(frame_index);
        if let Some(before) =
            self.virtual_generated_child(node, &style, css::PseudoElement::Before, available)?
        {
            self.append_virtual_generated_child(&before, &style, paragraph, frame_path)?;
        }
        let mut children = self
            .document
            .composed_children_iter(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(child) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            let child_style = if matches!(
                self.document
                    .kind(child)
                    .map_err(|_| LayoutError::InvalidTree)?,
                NodeKind::Element { .. }
            ) {
                Some(
                    self.computed_style(child, Some(&style))
                        .map_err(LayoutError::Css)?
                        .resolve_percentages(available, self.parent_height),
                )
            } else {
                None
            };
            self.collect_inline_paragraph_node(
                child,
                node,
                &style,
                child_style,
                available,
                paragraph,
                frame_path,
                depth + 1,
            )?;
        }
        if let Some(after) =
            self.virtual_generated_child(node, &style, css::PseudoElement::After, available)?
        {
            self.append_virtual_generated_child(&after, &style, paragraph, frame_path)?;
        }
        frame_path.pop();
        Ok(())
    }

    fn inline_paragraph_tokens(
        &self,
        paragraph: &InlineParagraph,
        bidi: &lumen_common::bidi::BidiInfo<'_>,
        paragraph_index: usize,
        range: Range<usize>,
    ) -> Result<Vec<InlineToken>, LayoutError> {
        if range.start >= range.end {
            return Ok(Vec::new());
        }
        let info = bidi
            .paragraphs
            .get(paragraph_index)
            .ok_or(LayoutError::Text)?;
        let (levels, runs) = bidi.visual_runs(info, range.clone());
        let mut tokens = Vec::new();
        for run in runs {
            let Some(level) = levels.get(run.start) else {
                continue;
            };
            let rtl = level.is_rtl();
            let mut parts: Vec<(usize, usize, InlinePart)> = Vec::new();
            let mut atom_index = paragraph
                .atoms
                .partition_point(|atom| atom.range.end <= run.start);
            while let Some(atom) = paragraph.atoms.get(atom_index) {
                if atom.range.start >= run.end {
                    break;
                }
                if atom.range.end > run.start {
                    if parts.len() >= MAX_DISPLAY_COMMANDS {
                        return Err(LayoutError::CommandLimit);
                    }
                    parts
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    parts.push((
                        atom.range.start,
                        atom.range.end,
                        InlinePart::Atom(atom_index),
                    ));
                }
                atom_index += 1;
            }
            let mut span_index = paragraph
                .spans
                .partition_point(|span| span.range.end <= run.start);
            while let Some(span) = paragraph.spans.get(span_index) {
                if span.range.start >= run.end {
                    break;
                }
                let start = span.range.start.max(run.start);
                let end = span.range.end.min(run.end);
                if start < end {
                    let value = paragraph.text.get(start..end).ok_or(LayoutError::Text)?;
                    let mut segment_start = None;
                    for (offset, character) in value.char_indices() {
                        let absolute = start + offset;
                        let stop = matches!(character, '\u{200b}' | '\u{2029}' | '\n' | '\r');
                        if stop {
                            if let Some(begin) = segment_start.take() {
                                if parts.len() >= MAX_DISPLAY_COMMANDS {
                                    return Err(LayoutError::CommandLimit);
                                }
                                parts
                                    .try_reserve(1)
                                    .map_err(|_| LayoutError::CommandLimit)?;
                                parts.push((
                                    begin,
                                    absolute,
                                    InlinePart::Text {
                                        span: span_index,
                                        range: begin..absolute,
                                    },
                                ));
                            }
                        } else if segment_start.is_none() {
                            segment_start = Some(absolute);
                        }
                    }
                    if let Some(begin) = segment_start {
                        if parts.len() >= MAX_DISPLAY_COMMANDS {
                            return Err(LayoutError::CommandLimit);
                        }
                        parts
                            .try_reserve(1)
                            .map_err(|_| LayoutError::CommandLimit)?;
                        parts.push((
                            begin,
                            end,
                            InlinePart::Text {
                                span: span_index,
                                range: begin..end,
                            },
                        ));
                    }
                }
                span_index += 1;
            }
            parts.sort_by_key(|(start, end, _)| (*start, *end));
            if rtl {
                parts.reverse();
            }
            for (_, _, part) in parts {
                match part {
                    InlinePart::Text { span, range } => {
                        let Some(source) = paragraph.text.get(range.clone()) else {
                            return Err(LayoutError::Text);
                        };
                        let span = paragraph.spans.get(span).ok_or(LayoutError::Text)?;
                        let shaped = self
                            .text
                            .shape_resolved(source, span.style.font_size, rtl, &span.style.font)
                            .map_err(|_| LayoutError::Text)?;
                        let mut frames = Vec::new();
                        frames
                            .try_reserve(span.frames.len())
                            .map_err(|_| LayoutError::CommandLimit)?;
                        frames.extend_from_slice(&span.frames);
                        if tokens.len() >= MAX_DISPLAY_COMMANDS {
                            return Err(LayoutError::CommandLimit);
                        }
                        tokens
                            .try_reserve(1)
                            .map_err(|_| LayoutError::CommandLimit)?;
                        tokens.push(InlineToken {
                            style: Some(span.style.clone()),
                            style_node: Some(span.style_node),
                            frames,
                            atom: None,
                            width: shaped.width,
                            shaped: Some(shaped),
                            source_range: Some(range),
                            rtl,
                            x: 0.0,
                            command: None,
                            hit: None,
                        });
                    }
                    InlinePart::Atom(atom_index) => {
                        let atom = paragraph.atoms.get(atom_index).ok_or(LayoutError::Text)?;
                        let mut frames = Vec::new();
                        frames
                            .try_reserve(atom.frames.len())
                            .map_err(|_| LayoutError::CommandLimit)?;
                        frames.extend_from_slice(&atom.frames);
                        if tokens.len() >= MAX_DISPLAY_COMMANDS {
                            return Err(LayoutError::CommandLimit);
                        }
                        tokens
                            .try_reserve(1)
                            .map_err(|_| LayoutError::CommandLimit)?;
                        tokens.push(InlineToken {
                            style: None,
                            style_node: None,
                            frames,
                            atom: Some(atom_index),
                            shaped: None,
                            source_range: None,
                            rtl,
                            width: atom.width,
                            x: 0.0,
                            command: None,
                            hit: None,
                        });
                    }
                }
            }
        }
        Ok(tokens)
    }

    fn inline_paragraph_width(
        &self,
        paragraph: &InlineParagraph,
        bidi: &lumen_common::bidi::BidiInfo<'_>,
        paragraph_index: usize,
        range: Range<usize>,
    ) -> Result<f32, LayoutError> {
        Ok(self
            .inline_paragraph_tokens(paragraph, bidi, paragraph_index, range)?
            .into_iter()
            .map(|token| token.width)
            .sum())
    }

    fn paragraph_visible_end(
        paragraph: &InlineParagraph,
        range: Range<usize>,
        hang: bool,
    ) -> usize {
        if !hang {
            return range.end;
        }
        let mut end = range.end;
        while end > range.start && paragraph.text.as_bytes().get(end - 1) == Some(&b' ') {
            let Some(span) = paragraph
                .spans
                .iter()
                .find(|span| span.range.start < end && span.range.end >= end)
            else {
                break;
            };
            if !matches!(
                span.style.white_space,
                WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine | WhiteSpace::PreWrap
            ) {
                break;
            }
            end -= 1;
        }
        end
    }

    fn flow_inline_paragraph(
        &mut self,
        paragraph: &mut InlineParagraph,
        block_style: &Style,
        left: f32,
        width: f32,
        cursor: &mut f32,
        advance: &mut f32,
        line_height: &mut f32,
        floats: &[(Rect, Float)],
        line: &mut LineStart,
        trailing: &mut f32,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if paragraph.text.is_empty() {
            if paragraph.frames.is_empty() {
                return Ok(());
            }
            let x = left + *advance;
            let mut height = self.line_height(block_style);
            for frame in &paragraph.frames {
                height = height.max(self.line_height(&frame.style));
            }
            for index in 0..paragraph.frames.len() {
                let node = paragraph.frames[index].node;
                let hit = if paragraph.frames[index].virtual_pseudo.is_some() {
                    self.begin_virtual_hit(node)?
                } else {
                    self.begin_hit(node)?
                };
                paragraph.frames[index].hit = hit;
                let frame_style = &paragraph.frames[index].style;
                if frame_style.visibility_visible
                    && has_background(frame_style)
                    && (*cursor < self.cull_bottom() && x < self.cull_right()
                        || self.transform_depth > 0)
                {
                    self.push_background(
                        Rect {
                            x,
                            y: *cursor,
                            width: 0.0,
                            height,
                        },
                        frame_style,
                    )?;
                }
                self.finish_hit(
                    hit,
                    Rect {
                        x,
                        y: *cursor,
                        width: 0.0,
                        height,
                    },
                );
            }
            *line_height = (*line_height).max(height);
            return Ok(());
        }
        if self.transform_depth == 0 && (*cursor >= self.cull_bottom() || left >= self.cull_right())
        {
            *line_height = (*line_height).max(self.line_height(block_style));
            return Ok(());
        }
        let bidi = lumen_common::bidi::resolve(
            &paragraph.text,
            Some(block_style.direction == Direction::Rtl),
        )
        .map_err(|_| LayoutError::Text)?;
        let mut breaks = Vec::new();
        for (offset, opportunity) in lumen_common::ucd::line_breaks(&paragraph.text) {
            if matches!(
                opportunity,
                lumen_common::ucd::BreakOpportunity::Allowed
                    | lumen_common::ucd::BreakOpportunity::Mandatory
            ) {
                breaks
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                breaks.push((
                    offset,
                    opportunity == lumen_common::ucd::BreakOpportunity::Mandatory,
                ));
            }
        }
        for &offset in &paragraph.hard_breaks {
            breaks
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            breaks.push((offset, true));
        }
        breaks.sort_by_key(|(offset, _)| *offset);
        let mut deduplicated = Vec::new();
        for (offset, mandatory) in breaks {
            if let Some((last, last_mandatory)) = deduplicated.last_mut() {
                if *last == offset {
                    *last_mandatory |= mandatory;
                    continue;
                }
            }
            deduplicated
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            deduplicated.push((offset, mandatory));
        }
        let breaks = deduplicated;
        let wrap = !matches!(
            block_style.white_space,
            WhiteSpace::NoWrap | WhiteSpace::Pre
        );
        let hang = matches!(
            block_style.white_space,
            WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine | WhiteSpace::PreWrap
        );

        if wrap && *advance > 0.0 {
            if let Some((first_break, _)) = breaks.iter().find(|(offset, _)| *offset > 0) {
                let visible_end = Self::paragraph_visible_end(paragraph, 0..*first_break, hang);
                let first_width =
                    self.inline_paragraph_width(paragraph, &bidi, 0, 0..visible_end)?;
                let height = (*line_height).max(self.line_height(block_style));
                let (_, available) = float_edges(floats, left, width, *cursor, height);
                if (*advance - *trailing).max(0.0) + first_width > available {
                    self.align_line(
                        *line,
                        left,
                        available,
                        *advance - *trailing,
                        block_style,
                        false,
                    );
                    *cursor += height;
                    *advance = 0.0;
                    *line_height = 0.0;
                    *trailing = 0.0;
                    *line = self.line_start();
                }
            }
        }

        let frame_hit_start = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        for index in 0..paragraph.frames.len() {
            let hit = if paragraph.frames[index].virtual_pseudo.is_some() {
                self.begin_virtual_hit(paragraph.frames[index].node)?
            } else {
                self.begin_hit(paragraph.frames[index].node)?
            };
            paragraph.frames[index].hit = hit;
        }
        let frame_hit_end = self
            .geometry
            .as_ref()
            .map_or(frame_hit_start, |geometry| geometry.hits.len());

        for paragraph_index in 0..bidi.paragraphs.len() {
            let paragraph_range = bidi.paragraphs[paragraph_index].range.clone();
            if paragraph_range.start >= paragraph_range.end {
                continue;
            }
            let mut start = paragraph_range.start;
            while start < paragraph_range.end {
                let forced_end = breaks
                    .iter()
                    .find(|(offset, mandatory)| *mandatory && *offset > start)
                    .map_or(paragraph_range.end, |(offset, _)| {
                        (*offset).min(paragraph_range.end)
                    });
                let mut candidates: Vec<usize> = breaks
                    .iter()
                    .filter_map(|(offset, _)| {
                        (*offset > start && *offset <= forced_end).then_some(*offset)
                    })
                    .collect();
                if candidates.last().copied() != Some(forced_end) {
                    candidates
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    candidates.push(forced_end);
                }
                if candidates.is_empty() {
                    break;
                }
                let mut height = (*line_height).max(self.line_height(block_style));
                let mut available = float_edges(floats, left, width, *cursor, height).1;
                let existing_visible = (*advance - *trailing).max(0.0);
                let capacity = (available - existing_visible).max(0.0);
                let mut chosen = forced_end;
                if wrap {
                    let mut token_start = start;
                    let mut accumulated = 0.0;
                    let mut last_fit = None;
                    for candidate in &candidates {
                        let token_width = self.inline_paragraph_width(
                            paragraph,
                            &bidi,
                            paragraph_index,
                            token_start..*candidate,
                        )?;
                        let token_visible_end =
                            Self::paragraph_visible_end(paragraph, token_start..*candidate, hang);
                        let token_visible = if token_visible_end == *candidate {
                            token_width
                        } else {
                            self.inline_paragraph_width(
                                paragraph,
                                &bidi,
                                paragraph_index,
                                token_start..token_visible_end,
                            )?
                        };
                        let candidate_full = accumulated + token_width;
                        let candidate_visible = candidate_full - token_width + token_visible;
                        if candidate_visible <= capacity {
                            last_fit = Some(*candidate);
                        } else if let Some(last_fit) = last_fit {
                            chosen = last_fit;
                            break;
                        } else {
                            chosen = *candidate;
                            break;
                        }
                        accumulated = candidate_full;
                        token_start = *candidate;
                    }
                }
                if chosen <= start {
                    chosen = forced_end.max(start + 1).min(paragraph_range.end);
                }
                let chosen_range = start..chosen;
                let mut tokens = self.inline_paragraph_tokens(
                    paragraph,
                    &bidi,
                    paragraph_index,
                    chosen_range.clone(),
                )?;
                let line_width: f32 = tokens.iter().map(|token| token.width).sum();
                let visible_end =
                    Self::paragraph_visible_end(paragraph, chosen_range.clone(), hang);
                let visible_width = if visible_end == chosen {
                    line_width
                } else {
                    self.inline_paragraph_width(
                        paragraph,
                        &bidi,
                        paragraph_index,
                        chosen_range.start..visible_end,
                    )?
                };
                for token in &tokens {
                    let token_height = match token.atom {
                        Some(atom) => paragraph.atoms[atom].height,
                        None => token
                            .style
                            .as_ref()
                            .map_or(self.line_height(block_style), |style| {
                                self.line_height(style)
                            }),
                    };
                    height = height.max(token_height);
                }
                let (mut line_left, next_available) =
                    float_edges(floats, left, width, *cursor, height);
                available = next_available;
                while wrap && *advance == 0.0 && available < visible_width && !floats.is_empty() {
                    let next = floats
                        .iter()
                        .map(|(rect, _)| rect.y + rect.height)
                        .filter(|end| *end > *cursor)
                        .min_by(f32::total_cmp);
                    let Some(next) = next else {
                        break;
                    };
                    *cursor = next;
                    (line_left, available) = float_edges(floats, left, width, *cursor, height);
                }
                let mut x = line_left + *advance;
                for token in &mut tokens {
                    token.x = x;
                    x += token.width;
                }
                let mut frame_line: Vec<Option<(f32, f32)>> = Vec::new();
                frame_line
                    .try_reserve(paragraph.frames.len())
                    .map_err(|_| LayoutError::CommandLimit)?;
                frame_line.resize(paragraph.frames.len(), None);
                for token in &tokens {
                    for frame in &token.frames {
                        if let Some(bounds) = frame_line.get_mut(*frame) {
                            *bounds =
                                Some(bounds.map_or((token.x, token.x + token.width), |prior| {
                                    (prior.0.min(token.x), prior.1.max(token.x + token.width))
                                }));
                        }
                    }
                }
                for (index, bounds) in frame_line.iter().enumerate() {
                    let Some((start_x, end_x)) = bounds else {
                        continue;
                    };
                    let frame_style = &paragraph.frames[index].style;
                    if frame_style.visibility_visible
                        && has_background(frame_style)
                        && (*cursor < self.cull_bottom() && *start_x < self.cull_right()
                            || self.transform_depth > 0)
                    {
                        self.push_background(
                            Rect {
                                x: *start_x,
                                y: *cursor,
                                width: (end_x - start_x).max(0.0),
                                height,
                            },
                            frame_style,
                        )?;
                    }
                }
                let first_command = line.command;
                let first_line_hit = self
                    .geometry
                    .as_ref()
                    .map_or(frame_hit_end, |geometry| geometry.hits.len());
                for token in &mut tokens {
                    if let Some(atom_index) = token.atom {
                        let atom = &paragraph.atoms[atom_index];
                        let hit_start = self
                            .geometry
                            .as_ref()
                            .map_or(0, |geometry| geometry.hits.len());
                        let decoration_depth = self.decorations.len();
                        for frame in &token.frames {
                            self.begin_decoration(&paragraph.frames[*frame].style)?;
                        }
                        let (actual_width, actual_height) = self.inline_for(
                            atom.node,
                            &atom.parent_style,
                            Some(atom.style.clone()),
                            token.x,
                            *cursor,
                            width,
                            depth + 1,
                        )?;
                        self.decorations.truncate(decoration_depth);
                        token.width = actual_width;
                        height = height.max(actual_height);
                        token.hit = (hit_start
                            ..self
                                .geometry
                                .as_ref()
                                .map_or(hit_start, |geometry| geometry.hits.len()))
                            .next();
                    } else if let (Some(style), Some(shaped)) = (&token.style, &token.shaped) {
                        if style.visibility_visible
                            && (self.transform_depth > 0
                                || token.x < self.cull_right() && *cursor < self.cull_bottom())
                        {
                            let command = self.commands.len();
                            self.push_command(Command::GlyphRun {
                                origin_x: token.x,
                                baseline_y: *cursor + self.baseline(style),
                                size: style.font_size,
                                color: style.color,
                                glyphs: shaped.glyphs.clone(),
                            })?;
                            token.command = Some(command);
                            let decoration_depth = self.decorations.len();
                            for frame in &token.frames {
                                if paragraph.frames[*frame].virtual_pseudo.is_some()
                                    || Some(paragraph.frames[*frame].node) != token.style_node
                                {
                                    self.begin_decoration(&paragraph.frames[*frame].style)?;
                                }
                            }
                            self.decorate_text(
                                token.x,
                                *cursor + self.baseline(style),
                                token.width,
                                style,
                            )?;
                            self.decorations.truncate(decoration_depth);
                        }
                    }
                }
                let used = (*advance - *trailing).max(0.0) + visible_width;
                let auto_wrapped = chosen < forced_end;
                let hard_break = paragraph.hard_breaks.contains(&chosen)
                    || breaks
                        .iter()
                        .any(|(offset, mandatory)| *offset == chosen && *mandatory)
                    || paragraph
                        .text
                        .get(start..chosen)
                        .is_some_and(|value| value.ends_with('\u{2029}'));
                let next_paragraph =
                    paragraph_index + 1 < bidi.paragraphs.len() && chosen == paragraph_range.end;
                let line_forced = hard_break || next_paragraph;
                let end = self.line_start();
                let line_first = LineStart {
                    command: first_command,
                    hit: first_line_hit,
                };
                let finalize_line = auto_wrapped || line_forced;
                if finalize_line {
                    self.align_line_until(
                        line_first,
                        end,
                        available,
                        used,
                        block_style,
                        !auto_wrapped,
                    );
                }
                let line_offset = if finalize_line {
                    Self::line_alignment_offset(available, used, block_style, !auto_wrapped)
                } else {
                    0.0
                };
                if line_offset != 0.0 && frame_hit_start > line.hit {
                    if let Some(geometry) = self.geometry.as_mut() {
                        if let Some(hits) = geometry.hits.get_mut(line.hit..frame_hit_start) {
                            for hit in hits {
                                hit.rect.x += line_offset;
                            }
                        }
                    }
                }
                let mut final_frame_line: Vec<Option<(f32, f32)>> = Vec::new();
                final_frame_line
                    .try_reserve(paragraph.frames.len())
                    .map_err(|_| LayoutError::CommandLimit)?;
                final_frame_line.resize(paragraph.frames.len(), None);
                for token in &tokens {
                    let token_x = token
                        .command
                        .and_then(|command| self.commands.get(command))
                        .and_then(|command| match command {
                            Command::GlyphRun { origin_x, .. } => Some(*origin_x),
                            _ => None,
                        })
                        .or_else(|| {
                            token.hit.and_then(|hit| {
                                self.geometry
                                    .as_ref()
                                    .and_then(|geometry| geometry.hits.get(hit))
                                    .map(|hit| hit.rect.x)
                            })
                        })
                        .unwrap_or(token.x + line_offset);
                    if let (Some(node), Some(range), Some(style)) = (
                        paragraph.control_node,
                        token.source_range.as_ref(),
                        token.style.as_ref(),
                    ) {
                        if token.command.is_some() {
                            let baseline_y = *cursor + self.baseline(style);
                            let source_range = control_source_range(
                                range,
                                paragraph.control_source_ranges.as_deref(),
                                paragraph.control_placeholder,
                            )?;
                            if let Some(geometry) = self.geometry.as_mut() {
                                if geometry.control_text_runs.len() >= MAX_DISPLAY_COMMANDS {
                                    return Err(LayoutError::CommandLimit);
                                }
                                geometry
                                    .control_text_runs
                                    .try_reserve(1)
                                    .map_err(|_| LayoutError::CommandLimit)?;
                                geometry.control_text_runs.push(ControlTextRun {
                                    node,
                                    range: source_range,
                                    display_range: range.clone(),
                                    hit_index: paragraph.control_hit_index,
                                    command_index: token.command,
                                    x: token_x,
                                    y: *cursor,
                                    width: token.width,
                                    height,
                                    baseline_y,
                                    rtl: token.rtl,
                                    font_size: style.font_size,
                                    font: style.font.clone(),
                                    placeholder: paragraph.control_placeholder,
                                    password: paragraph.control_password,
                                    transform: crate::paint::Affine::IDENTITY,
                                });
                            }
                        }
                    }
                    for frame in &token.frames {
                        if let Some(bounds) = final_frame_line.get_mut(*frame) {
                            *bounds =
                                Some(bounds.map_or((token_x, token_x + token.width), |prior| {
                                    (prior.0.min(token_x), prior.1.max(token_x + token.width))
                                }));
                        }
                    }
                }
                for (index, bounds) in final_frame_line.into_iter().enumerate() {
                    let Some((start_x, end_x)) = bounds else {
                        continue;
                    };
                    let fragment = Rect {
                        x: start_x,
                        y: *cursor,
                        width: (end_x - start_x).max(0.0),
                        height,
                    };
                    let frame = &mut paragraph.frames[index];
                    frame.bounds = Some(frame.bounds.map_or(fragment, |prior| Rect {
                        x: prior.x.min(fragment.x),
                        y: prior.y.min(fragment.y),
                        width: (prior.x + prior.width).max(fragment.x + fragment.width)
                            - prior.x.min(fragment.x),
                        height: (prior.y + prior.height).max(fragment.y + fragment.height)
                            - prior.y.min(fragment.y),
                    }));
                }
                if finalize_line {
                    *cursor += height;
                    *advance = 0.0;
                    *line_height = 0.0;
                    *trailing = 0.0;
                    *line = self.line_start();
                } else {
                    *advance += line_width;
                    *line_height = (*line_height).max(height);
                    *trailing = (line_width - visible_width).max(0.0);
                }
                start = chosen;
                if chosen == paragraph_range.end {
                    break;
                }
            }
        }
        for frame in &paragraph.frames {
            self.finish_hit(
                frame.hit,
                frame.bounds.unwrap_or(Rect {
                    x: left + *advance,
                    y: *cursor,
                    width: 0.0,
                    height: 0.0,
                }),
            );
        }
        Ok(())
    }

    fn line_alignment_offset(available: f32, used: f32, style: &Style, last: bool) -> f32 {
        if used <= 0.0 {
            return 0.0;
        }
        let free = (available - used).max(0.0);
        let right = match style.text_align {
            TextAlign::Right => true,
            TextAlign::Start => style.direction == Direction::Rtl,
            TextAlign::End => style.direction == Direction::Ltr,
            TextAlign::Justify => last && style.direction == Direction::Rtl,
            _ => false,
        };
        if right {
            free
        } else if style.text_align == TextAlign::Center {
            free * 0.5
        } else {
            0.0
        }
    }

    fn begin_hit(&mut self, node: NodeId) -> Result<Option<usize>, LayoutError> {
        self.begin_hit_with_kind(node, false)
    }

    fn begin_virtual_hit(&mut self, node: NodeId) -> Result<Option<usize>, LayoutError> {
        self.begin_hit_with_kind(node, true)
    }

    fn begin_hit_with_kind(
        &mut self,
        node: NodeId,
        virtual_generated: bool,
    ) -> Result<Option<usize>, LayoutError> {
        let Some(geometry) = self.geometry.as_mut() else {
            return Ok(None);
        };
        let hits = &mut geometry.hits;
        if hits.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        hits.try_reserve(1).map_err(|_| LayoutError::CommandLimit)?;
        let index = hits.len();
        hits.push(HitRegion {
            node,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            virtual_generated,
        });
        Ok(Some(index))
    }

    fn finish_hit(&mut self, index: Option<usize>, rect: Rect) {
        if let (Some(geometry), Some(index)) = (self.geometry.as_mut(), index) {
            geometry.hits[index].rect = rect;
        }
    }
    fn push_command(&mut self, command: Command) -> Result<(), LayoutError> {
        if self.commands.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        let bytes = lumen_common::limits::size::sum(
            self.command_bytes,
            command.referenced_bytes(),
            MAX_DISPLAY_LIST_BYTES,
        )
        .map_err(|_| LayoutError::CommandLimit)?;
        self.commands
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        self.commands.push(command);
        self.command_bytes = bytes;
        Ok(())
    }

    fn replace_command(&mut self, index: usize, command: Command) -> Result<(), LayoutError> {
        let current = self
            .commands
            .get_mut(index)
            .ok_or(LayoutError::InvalidTree)?;
        replace_command_bytes(current, &mut self.command_bytes, command)
    }

    fn pop_command(&mut self) {
        if let Some(command) = self.commands.pop() {
            self.command_bytes = self
                .command_bytes
                .saturating_sub(command.referenced_bytes());
        }
    }

    fn finish_display_list(&mut self) -> Result<DisplayList, LayoutError> {
        self.commands.shrink_to_fit();
        let list = DisplayList(core::mem::take(&mut self.commands));
        lumen_common::limits::size::check(list.referenced_bytes(), MAX_DISPLAY_LIST_BYTES)
            .map_err(|_| LayoutError::CommandLimit)?;
        Ok(list)
    }

    fn text_flow(
        &mut self,
        value: &str,
        style: &Style,
        left: f32,
        width: f32,
        cursor: &mut f32,
        advance: &mut f32,
        line_height: &mut f32,
        floats: &[(Rect, Float)],
        line: &mut LineStart,
        trailing: &mut f32,
    ) -> Result<(), LayoutError> {
        let collapse = matches!(
            style.white_space,
            WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine
        );
        let wrap = !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre);
        let normalized = formatted_text(value, style.white_space);
        let value = if (*advance == 0.0 || *trailing > 0.0) && collapse {
            normalized.trim_start_matches(' ')
        } else {
            &normalized
        };
        if value.is_empty() {
            return Ok(());
        }
        let size = style.font_size;
        let height = self.line_height(style);
        if *cursor >= self.cull_bottom() || left >= self.cull_right() {
            *line_height = (*line_height).max(height);
            return Ok(());
        }
        if floats.is_empty()
            && !style.text_align.justifies()
            && !(*advance == 0.0 && value.starts_with(' '))
            && !value
                .bytes()
                .any(|byte| matches!(byte, b'\n' | b'\r' | b'\t'))
        {
            let shaped = self
                .text
                .shape_styled(value, size, style.direction == Direction::Rtl, &style.font)
                .map_err(|_| LayoutError::Text)?;
            if !wrap || *advance + shaped.width <= width {
                let tail = if collapse {
                    value.len() - value.trim_end_matches(' ').len()
                } else {
                    0
                };
                *trailing = if tail == 0 {
                    0.0
                } else {
                    self.text
                        .measure_styled(&value[value.len() - tail..], size, &style.font)
                        .map_err(|_| LayoutError::Text)?
                };
                if style.visibility_visible
                    && left + *advance < self.cull_right()
                    && *cursor < self.cull_bottom()
                {
                    self.push_command(Command::GlyphRun {
                        origin_x: left + *advance,
                        baseline_y: *cursor + self.baseline(style),
                        size,
                        color: style.color,
                        glyphs: shaped.glyphs,
                    })?;
                    self.decorate_text(
                        left + *advance,
                        *cursor + self.baseline(style),
                        shaped.width,
                        style,
                    )?;
                }
                *advance += shaped.width;
                *line_height = (*line_height).max(height);
                return Ok(());
            }
        }
        let chunks = lumen_common::ucd::line_breaks(value).scan(0, |start, (end, _)| {
            let part = &value[*start..end];
            *start = end;
            Some(part)
        });
        for part in chunks
            .flat_map(|part| part.split_inclusive(' '))
            .flat_map(|part| part.split_inclusive('\t'))
        {
            if collapse && part.trim_matches(' ').is_empty() && *advance == 0.0 {
                continue;
            }
            let newline = part.ends_with('\n');
            let tab = part.ends_with('\t');
            let part = part.trim_end_matches(['\n', '\t']);
            let shaped = self
                .text
                .shape_styled(part, size, style.direction == Direction::Rtl, &style.font)
                .map_err(|_| LayoutError::Text)?;
            let (mut line_left, mut space) = float_edges(floats, left, width, *cursor, height);
            let hang = collapse || style.white_space == WhiteSpace::PreWrap;
            let visible_width = if hang && part.ends_with(' ') {
                let visible = part.trim_end_matches(' ');
                if visible.is_empty() {
                    0.0
                } else {
                    self.text
                        .measure_styled(visible, size, &style.font)
                        .map_err(|_| LayoutError::Text)?
                }
            } else {
                shaped.width
            };
            if wrap && visible_width > 0.0 && *advance > 0.0 && *advance + visible_width > space {
                self.align_line(*line, left, space, *advance - *trailing, style, false);
                *cursor += *line_height;
                *advance = 0.0;
                *line_height = 0.0;
                *line = self.line_start();
                *trailing = 0.0;
                if collapse && part.trim_matches(' ').is_empty() {
                    continue;
                }
                (line_left, space) = float_edges(floats, left, width, *cursor, height);
            }
            while wrap && *advance == 0.0 && space < visible_width && !floats.is_empty() {
                let next = floats
                    .iter()
                    .map(|(rect, _)| rect.y + rect.height)
                    .filter(|end| *end > *cursor)
                    .min_by(f32::total_cmp);
                let Some(next) = next else {
                    break;
                };
                *cursor = next;
                (line_left, space) = float_edges(floats, left, width, *cursor, height);
            }
            if style.visibility_visible
                && !part.is_empty()
                && line_left + *advance < self.cull_right()
                && *cursor < self.cull_bottom()
            {
                self.push_command(Command::GlyphRun {
                    origin_x: line_left + *advance,
                    baseline_y: *cursor + self.baseline(style),
                    size,
                    color: style.color,
                    glyphs: shaped.glyphs,
                })?;
                self.decorate_text(
                    line_left + *advance,
                    *cursor + self.baseline(style),
                    visible_width,
                    style,
                )?;
            }
            *advance += shaped.width;
            if tab {
                let tab_width = (self
                    .text
                    .measure_styled(" ", size, &style.font)
                    .map_err(|_| LayoutError::Text)?
                    * 8.0)
                    .max(1.0);
                *advance = ((*advance / tab_width).floor() + 1.0) * tab_width;
            }
            *trailing = if hang {
                shaped.width - visible_width
            } else {
                0.0
            };
            *line_height = (*line_height).max(height);
            if newline {
                self.align_line(*line, line_left, space, *advance - *trailing, style, true);
                *cursor += *line_height;
                *advance = 0.0;
                *line_height = 0.0;
                *trailing = 0.0;
                *line = self.line_start();
            }
        }
        Ok(())
    }

    fn line_start(&self) -> LineStart {
        LineStart {
            command: self.commands.len(),
            hit: self
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len()),
        }
    }

    fn align_line(
        &mut self,
        first: LineStart,
        _left: f32,
        available: f32,
        used: f32,
        style: &Style,
        last: bool,
    ) {
        self.align_line_until(first, self.line_start(), available, used, style, last);
    }

    fn align_line_until(
        &mut self,
        first: LineStart,
        end: LineStart,
        available: f32,
        used: f32,
        style: &Style,
        last: bool,
    ) {
        if used <= 0.0 {
            return;
        }
        let free = (available - used).max(0.0);
        let right = match style.text_align {
            TextAlign::Right => true,
            TextAlign::Start => style.direction == Direction::Rtl,
            TextAlign::End => style.direction == Direction::Ltr,
            TextAlign::Justify => last && style.direction == Direction::Rtl,
            _ => false,
        };
        let offset = if right {
            free
        } else if style.text_align == TextAlign::Center {
            free * 0.5
        } else {
            0.0
        };
        let count = if style.text_align.justifies()
            && (style.text_align == TextAlign::JustifyAll || !last)
        {
            self.commands[first.command..end.command]
                .iter()
                .filter(|c| matches!(c, Command::GlyphRun { .. }))
                .count()
                .saturating_sub(1)
        } else {
            0
        };
        if offset == 0.0 && count == 0 {
            return;
        }
        let mut word = 0usize;
        let mut seen = false;
        let line_y = self.commands[first.command..end.command]
            .iter()
            .find_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y - self.baseline(style)),
                _ => None,
            });
        for (index, command) in self.commands[first.command..end.command]
            .iter_mut()
            .enumerate()
        {
            if matches!(command, Command::GlyphRun { .. }) {
                if seen {
                    word += 1;
                }
                seen = true;
            }
            let dx = offset
                + if count != 0 {
                    free * word.min(count) as f32 / count as f32
                } else {
                    0.0
                };
            let is_glyph = matches!(command, Command::GlyphRun { .. });
            move_command(command, dx, 0.0);
            if dx != 0.0 && is_glyph {
                if let Some(geometry) = &mut self.geometry {
                    let command_index = first.command + index;
                    for run in &mut geometry.control_text_runs {
                        if run.command_index == Some(command_index) {
                            run.x += dx;
                        }
                    }
                }
            }
        }
        let height = self.line_height(style);
        if count == 0 && offset != 0.0 {
            if let (Some(geometry), Some(y)) = (&mut self.geometry, line_y) {
                if let Some(hits) = geometry.hits.get_mut(first.hit..end.hit) {
                    for hit in hits {
                        if hit.rect.y >= y && hit.rect.y + hit.rect.height <= y + height {
                            hit.rect.x += offset;
                        }
                    }
                }
            }
        }
    }

    fn inline_for(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        let computed = self
            .opacity_style(id, parent_style, computed)?
            .map(|style| style.resolve_percentages(available, self.parent_height));
        if computed
            .as_ref()
            .is_some_and(|style| matches!(style.position, Position::Absolute | Position::Fixed))
        {
            self.box_for(id, parent_style, computed, x, y, available, depth)?;
            return Ok((0.0, 0.0));
        }
        // Atomic inline-level box: lay out the interior like a block and
        // advance the line by its margin box (CSS 2.1 §9.2.2).
        if computed
            .as_ref()
            .is_some_and(|style| style.display == Display::InlineBlock)
        {
            let first_hit = self
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len());
            let first_command = self.commands.len();
            let margins = computed
                .as_ref()
                .map_or([0.0; 4], |style| style.margin_sides);
            let height = self.box_for(id, parent_style, computed, x, y, available, depth)?;
            let width = self
                .geometry
                .as_ref()
                .and_then(|geometry| geometry.hits.get(first_hit))
                .map(|hit| hit.rect.width)
                .or_else(|| {
                    self.commands[first_command..]
                        .iter()
                        .find_map(|command| match command {
                            Command::FillRect { rect, .. } => Some(rect.width),
                            Command::FillBackground(fill) => Some(fill.rect.width),
                            Command::MaskedBackground(mask) => Some(mask.rect.width),
                            _ => None,
                        })
                })
                .unwrap_or(0.0);
            return Ok((width + margins[1] + margins[3], height));
        }
        if matches!(
            self.document.kind(id),
            Ok(NodeKind::Element {
                name,
                namespace: Namespace::Svg,
                ..
            }) if crate::svg::local_name(name) == "svg"
        ) {
            let style = match computed.clone() {
                Some(style) => style,
                None => self
                    .computed_style(id, Some(parent_style))
                    .map_err(LayoutError::Css)?,
            };
            let (width, _) = self.intrinsic_size(id, &style, depth + 1)?;
            let margins = style.margin_sides;
            let height = self.box_for(id, parent_style, Some(style), x, y, available, depth)?;
            return Ok((width + margins[1] + margins[3], height));
        }
        if matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "img" || name == "canvas" || name == "video")
        {
            let first_hit = self
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len());
            let first_command = self.commands.len();
            let margins = computed
                .as_ref()
                .map_or([0.0; 4], |style| style.margin_sides);
            let edges = computed
                .as_ref()
                .map_or(0.0, |style| box_edges(style, true));
            let height = self.box_for(id, parent_style, computed, x, y, available, depth)?;
            let width = self
                .geometry
                .as_ref()
                .and_then(|geometry| geometry.hits.get(first_hit))
                .map(|hit| hit.rect.width)
                .or_else(|| {
                    self.commands[first_command..]
                        .iter()
                        .find_map(|command| match command {
                            Command::Image { rect, .. } => Some(rect.width + edges),
                            _ => None,
                        })
                })
                .unwrap_or(0.0);
            return Ok((width + margins[1] + margins[3], height));
        }
        let decorations = self.decorations.len();
        if let Some(style) = &computed {
            self.begin_decoration(style)?;
        }
        let offset = computed
            .as_ref()
            .filter(|style| {
                style.position == Position::Relative && style.display == Display::Inline
            })
            .map_or((0.0, 0.0), |style| {
                (
                    style.left.unwrap_or_else(|| -style.right.unwrap_or(0.0)),
                    style.top.unwrap_or_else(|| -style.bottom.unwrap_or(0.0)),
                )
            });
        let layer = self.begin_opacity(computed.as_ref().filter(|style| {
            !matches!(
                style.display,
                Display::Block | Display::Flex | Display::Grid
            )
        }))?;
        let result = self.inline_content(
            id,
            parent_style,
            computed,
            x + offset.0,
            y + offset.1,
            available,
            depth,
        );
        self.decorations.truncate(decorations);
        let result = result?;
        self.end_opacity(layer)?;
        Ok(result)
    }

    fn inline_content(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            let normalized = formatted_text(value, parent_style.white_space);
            let value = normalized.as_ref();
            if value.is_empty() {
                return Ok((0.0, 0.0));
            }
            let size = parent_style.font_size;
            let height = self.line_height(parent_style);
            if self.transform_depth == 0 && (x >= self.cull_right() || y >= self.cull_bottom()) {
                return Ok((0.0, height));
            }
            let shaped = self
                .text
                .shape_styled(
                    value,
                    size,
                    parent_style.direction == Direction::Rtl,
                    &parent_style.font,
                )
                .map_err(|_| LayoutError::Text)?;
            let width = shaped.width;
            if parent_style.visibility_visible {
                self.push_command(Command::GlyphRun {
                    origin_x: x,
                    baseline_y: y + self.baseline(parent_style),
                    size,
                    color: parent_style.color,
                    glyphs: shaped.glyphs,
                })?;
                self.decorate_text(x, y + self.baseline(parent_style), width, parent_style)?;
            }
            return Ok((width, height));
        }
        let NodeKind::Element {
            name, namespace, ..
        } = kind
        else {
            return Ok((0.0, 0.0));
        };
        if *namespace != Namespace::Html {
            return Ok((0.0, 0.0));
        }
        if matches!(
            name.as_str(),
            "head" | "style" | "script" | "meta" | "link" | "template"
        ) {
            return Ok((0.0, 0.0));
        }
        let style = match computed {
            Some(style) => style,
            None => self
                .computed_style(id, Some(parent_style))
                .map_err(LayoutError::Css)?,
        };
        if style.display == Display::None {
            return Ok((0.0, 0.0));
        }
        if has_layer_attachment(&style, css::BackgroundAttachment::Fixed) {
            self.scroll_hazards += 1;
        }
        if matches!(
            style.display,
            Display::Block | Display::Flex | Display::Grid
        ) {
            let height = self.box_for(id, parent_style, Some(style), x, y, available, depth)?;
            return Ok((available, height));
        }
        for pseudo in [css::PseudoElement::Before, css::PseudoElement::After] {
            if self
                .virtual_generated_child(id, &style, pseudo, available)?
                .is_some()
            {
                return Err(LayoutError::UnsupportedGeneratedContent);
            }
        }
        let hit = self.begin_hit(id)?;
        let margin = style.margin.max(0.0);
        let padding = style.padding.max(0.0);
        let border_width = border_widths(&style).into_iter().fold(0.0f32, f32::max);
        let outer_x = x + margin;
        let outer_y = y;
        let inset = padding + border_width;
        let background_index = if style.visibility_visible
            && has_background(&style)
            && (self.transform_depth > 0 || (outer_x < self.cull_right() && y < self.cull_bottom()))
        {
            let index = self.commands.len() + background_paints(&style) - 1;
            self.push_background(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: 0.0,
                    height: 0.0,
                },
                &style,
            )?;
            Some(index)
        } else {
            None
        };
        let border_index = if style.visibility_visible
            && !has_nonuniform_border(&style)
            && border_width > 0.0
            && style.border_color.a > 0
            && (self.transform_depth > 0 || (outer_x < self.cull_right() && y < self.cull_bottom()))
        {
            let index = self.commands.len();
            self.push_command(border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: 0.0,
                    height: 0.0,
                },
                &style,
            ))?;
            Some(index)
        } else {
            None
        };
        let mut advance = 0.0;
        let mut height = self.line_height(&style);
        let mut children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            let (width, child_height) = self.inline_for(
                current,
                &style,
                None,
                outer_x + inset + advance,
                outer_y + inset,
                available,
                depth + 1,
            )?;
            advance += width;
            height = height.max(child_height);
        }
        let box_width = advance + 2.0 * inset;
        let box_height = height + 2.0 * inset;
        let rect = Rect {
            x: outer_x,
            y: outer_y,
            width: box_width,
            height: box_height,
        };
        self.finish_hit(hit, rect);
        if let Some(index) = background_index {
            self.finish_background(index, rect, &style, None)?;
        }
        if let Some(index) = border_index {
            self.replace_command(index, border(rect, &style))?;
        } else if style.visibility_visible && has_nonuniform_border(&style) {
            for command in side_border_commands(rect, &style) {
                self.push_command(command)?;
            }
        }
        Ok((box_width + 2.0 * margin, box_height))
    }

    fn opacity_style(
        &self,
        id: NodeId,
        parent: &Style,
        computed: Option<Style>,
    ) -> Result<Option<Style>, LayoutError> {
        if computed.is_some() {
            return Ok(computed);
        }
        if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
            Ok(Some(
                self.computed_style(id, Some(parent))
                    .map_err(LayoutError::Css)?,
            ))
        } else {
            Ok(None)
        }
    }

    fn begin_opacity(&mut self, style: Option<&Style>) -> Result<Option<usize>, LayoutError> {
        if let Some(style) = style.filter(|style| style.opacity < 1.0) {
            let index = self.commands.len();
            self.push_command(Command::PushLayer {
                corners: None,
                rect: self.viewport,
                radius: 0.0,
                opacity: style.opacity,
                clip: false,
            })?;
            Ok(Some(index))
        } else {
            Ok(None)
        }
    }

    fn refresh_transform(
        &mut self,
        style: &Style,
        commands: core::ops::Range<usize>,
        hits: core::ops::Range<usize>,
    ) -> Result<(), LayoutError> {
        if style.transforms.is_none() {
            return Ok(());
        }
        let rect = self
            .geometry
            .as_ref()
            .and_then(|geometry| geometry.hits.get(hits.start))
            .map(|hit| hit.rect)
            .or_else(|| {
                self.commands[commands.clone()]
                    .iter()
                    .find_map(|command| match command {
                        Command::MaskedBackground(mask) => Some(mask.rect),
                        Command::StrokeBoxBorder(border) => Some(border.rect),
                        Command::FillRect { rect, .. }
                        | Command::StrokeBorder { rect, .. }
                        | Command::StrokePatternBorder { rect, .. }
                        | Command::Image { rect, .. } => Some(*rect),
                        Command::FillRoundedRect { rect, .. } => {
                            let inset = background_bleed_inset(style);
                            Some(Rect {
                                x: rect.x - inset,
                                y: rect.y - inset,
                                width: rect.width + 2.0 * inset,
                                height: rect.height + 2.0 * inset,
                            })
                        }
                        Command::FillBackground(fill) => {
                            let rect = &fill.rect;
                            let border = used_border(style);
                            Some(Rect {
                                x: rect.x - border,
                                y: rect.y - border,
                                width: rect.width + 2.0 * border,
                                height: rect.height + 2.0 * border,
                            })
                        }
                        Command::BoxShadow { rect, shadow, .. } if !shadow.inset => Some(*rect),
                        _ => None,
                    })
            });
        if let Some(rect) = rect {
            let matrix = style
                .transform_matrix(rect)
                .ok_or(LayoutError::Css(css::CssError {
                    offset: 0,
                    message: "invalid computed transform",
                }))?;
            if let Some(Command::PushTransform(value)) = self.commands.get_mut(commands.start) {
                *value = matrix;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if let Some(group) = geometry
                    .transforms
                    .iter_mut()
                    .rev()
                    .find(|group| group.hits == hits)
                {
                    group.matrix = matrix;
                }
                geometry.refresh_control_run_transforms(hits.clone());
            }
        }
        Ok(())
    }

    fn end_opacity(&mut self, layer: Option<usize>) -> Result<(), LayoutError> {
        if let Some(index) = layer {
            if self.commands.len() == index + 1 {
                self.pop_command();
            } else {
                self.push_command(Command::PopLayer)?;
            }
        }
        Ok(())
    }

    fn raise_flow_paint(
        &mut self,
        chunks: &mut [(core::ops::Range<usize>, core::ops::Range<usize>)],
    ) {
        for index in 0..chunks.len() {
            let (commands, hits) = chunks[index].clone();
            let command_end = self.commands.len();
            let hit_end = self.geometry.as_ref().map_or(0, |g| g.hits.len());
            self.commands[commands.start..command_end].rotate_left(commands.len());
            if let Some(geometry) = self.geometry.as_mut() {
                geometry.hits[hits.start..hit_end].rotate_left(hits.len());
                for transform in &mut geometry.transforms {
                    raise_range(&mut transform.hits, hits.start, hits.end, hit_end);
                }
                for clip in &mut geometry.rounded_clips {
                    raise_range(&mut clip.hits, hits.start, hits.end, hit_end);
                }
                for region in &mut geometry.scroll_regions {
                    raise_range(&mut region.hits, hits.start, hits.end, hit_end);
                    raise_range(
                        &mut region.commands,
                        commands.start,
                        commands.end,
                        command_end,
                    );
                }
            }
            for (command_range, hit_range) in &mut chunks[index + 1..] {
                raise_range(command_range, commands.start, commands.end, command_end);
                raise_range(hit_range, hits.start, hits.end, hit_end);
            }
            for entry in &mut self.pending_escapes {
                let mut range = entry.2..entry.3;
                raise_range(&mut range, commands.start, commands.end, command_end);
                (entry.2, entry.3) = (range.start, range.end);
                let mut range = entry.4..entry.5;
                raise_range(&mut range, hits.start, hits.end, hit_end);
                (entry.4, entry.5) = (range.start, range.end);
            }
            for paint in &mut self.positioned_flow_paints {
                raise_range(
                    &mut paint.commands,
                    commands.start,
                    commands.end,
                    command_end,
                );
                raise_range(&mut paint.hits, hits.start, hits.end, hit_end);
            }
        }
    }

    /// Pre-lays out absolutely/fixed positioned children with a negative
    /// z-index and fully specified insets (CSS 2.1 Appendix E layer 2). Their
    /// commands land before the container's own background when the container
    /// is not a stacking context (the child "escapes" below in-flow
    /// backgrounds) and after background/border when it is. Positions are
    /// resolved against the containing block, so the flow cursor is unused.
    fn pre_layout_negative_z(
        &mut self,
        id: NodeId,
        container: &Style,
        content_x: f32,
        cursor: f32,
        content_width: f32,
        depth: usize,
        laid: &mut Vec<NodeId>,
        record_escapes: bool,
    ) -> Result<(), LayoutError> {
        let mut children = self
            .document
            .composed_children_iter(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = children.next().map_err(|_| LayoutError::InvalidTree)? {
            if !matches!(
                self.document.kind(current),
                Ok(NodeKind::Element {
                    namespace: Namespace::Html,
                    ..
                })
            ) {
                continue;
            }
            let child = self
                .computed_style(current, Some(container))
                .map_err(LayoutError::Css)?;
            if !matches!(child.position, Position::Absolute | Position::Fixed) {
                continue;
            }
            if !child.z_index.is_some_and(|z| z < 0) {
                continue;
            }
            let has_vertical = child.top.is_some() || child.bottom.is_some();
            let has_horizontal = child.left.is_some() || child.right.is_some();
            if !(has_vertical && has_horizontal) {
                // The static position depends on the flow; paint it in the
                // positioned pass instead of guessing here.
                continue;
            }
            if laid.len() == 256 {
                return Err(LayoutError::CommandLimit);
            }
            laid.push(current);
            let first_cmd = self.commands.len();
            let hit_first = self
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len());
            self.box_for(
                current,
                container,
                Some(child),
                content_x,
                cursor,
                content_width,
                depth + 1,
            )?;
            if record_escapes {
                let target = self.stacking_roots.last().copied().unwrap_or((0, 0));
                self.pending_escapes.push((
                    target.0,
                    target.1,
                    first_cmd,
                    self.commands.len(),
                    hit_first,
                    self.geometry
                        .as_ref()
                        .map_or(0, |geometry| geometry.hits.len()),
                ));
            }
        }
        Ok(())
    }

    /// Moves recorded negative-z subtrees from their laid-out position to the
    /// insertion point of their stacking context, in paint order. Command and
    /// hit ranges are spliced; ranges recorded in the geometry are adjusted.
    fn rotate_stacking_escapes(&mut self, target_cmd: usize, target_hit: usize) {
        let mut mine: Vec<(usize, usize, usize, usize, usize, usize)> = Vec::new();
        let mut kept = Vec::new();
        for entry in self.pending_escapes.drain(..) {
            if entry.0 == target_cmd && entry.1 == target_hit {
                mine.push(entry);
            } else {
                kept.push(entry);
            }
        }
        self.pending_escapes = kept;
        mine.sort_by_key(|entry| entry.2);
        for (_, _, first, last, hit_first, hit_last) in mine {
            let len = last.saturating_sub(first);
            let hit_len = hit_last.saturating_sub(hit_first);
            if len == 0 || first <= target_cmd {
                continue;
            }
            let block: Vec<Command> = self.commands.drain(first..last).collect();
            let at = target_cmd.min(self.commands.len());
            for command in block.into_iter().rev() {
                self.commands.insert(at, command);
            }
            let hit_block: Vec<HitRegion> = self.geometry.as_mut().map_or(Vec::new(), |geometry| {
                geometry.hits.drain(hit_first..hit_last).collect()
            });
            if hit_len > 0 {
                if let Some(geometry) = self.geometry.as_mut() {
                    let at = target_hit.min(geometry.hits.len());
                    for hit in hit_block.into_iter().rev() {
                        geometry.hits.insert(at, hit);
                    }
                    for transform in &mut geometry.transforms {
                        shift_range(&mut transform.hits, hit_first, hit_last, target_hit);
                    }
                    for clip in &mut geometry.rounded_clips {
                        shift_range(&mut clip.hits, hit_first, hit_last, target_hit);
                    }
                    for region in &mut geometry.scroll_regions {
                        shift_region(&mut region.hits, hit_first, hit_last, target_hit);
                    }
                }
            }
            if let Some(geometry) = self.geometry.as_mut() {
                for region in &mut geometry.scroll_regions {
                    shift_region(&mut region.commands, first, last, at);
                }
            }
            for entry in &mut self.pending_escapes {
                shift_index(&mut entry.2, first, last, at);
                shift_index(&mut entry.3, first, last, at);
            }
            for paint in &mut self.positioned_flow_paints {
                shift_range(&mut paint.commands, first, last, at);
                shift_range(&mut paint.hits, hit_first, hit_last, target_hit);
            }
        }
    }

    fn retained_fragment_key(
        &self,
        id: NodeId,
        parent_style: &Style,
        computed: &Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Option<RetainedFragmentKey> {
        let style = computed.as_ref()?;
        let name = match self.document.kind(id).ok()? {
            NodeKind::Element { name, .. } => name,
            _ => return None,
        };
        // Keep the retained path deliberately narrow: only ordinary block
        // boxes at an exact layout position are independent of formatting
        // context bookkeeping. More complex formatting stays on the normal
        // layout path and can still reuse safe block descendants.
        if depth == 0
            || matches!(name.as_str(), "html" | "body")
            || parent_style.display != Display::Block
            || style.display != Display::Block
            || style.position != Position::Static
            || style.float != Float::None
            || style.opacity < 1.0
            || style.z_index.is_some()
            || style.transforms.is_some()
            || style.overflow_clip
            || style.overflow_x != css::Overflow::Visible
            || style.overflow_y != css::Overflow::Visible
            || style.column_count.is_some()
            || style.sibling_position_dependent
            || style.background_images.is_some()
            || style.background_attachment.as_ref().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| *item != css::BackgroundAttachment::Scroll)
            })
            || self.transform_depth != 0
            || !self.pending_escapes.is_empty()
            || !self.positioned_flow_paints.is_empty()
            || self.stacking_roots.len() > 1
            || self.geometry.is_none()
        {
            return None;
        }
        Some(RetainedFragmentKey {
            node: id,
            parent_style: parent_style.clone(),
            style: computed.clone(),
            x,
            y,
            available,
            depth,
            parent_height: self.parent_height,
            containing_block: self.containing_block,
            fixed_containing_block: self.fixed_containing_block,
            sticky_bounds: self.sticky_bounds,
            cull: self.cull,
            viewport: self.viewport,
            scroll_hazards: self.scroll_hazards,
            transform_depth: self.transform_depth,
            next_paint_order: self.next_paint_order,
            suppressed_border_node: self.suppressed_border_node,
            body_background_on_canvas: self.body_background_on_canvas,
            collapsed_bottom: self.collapsed_bottom,
            was_through: self.was_through,
            decorations: self.decorations.clone(),
        })
    }

    fn replay_retained_fragment(&mut self, fragment: &RetainedFragment) -> Result<(), LayoutError> {
        for command in fragment.commands.iter() {
            self.push_command(command.clone())?;
        }
        let Some(geometry) = self.geometry.as_deref_mut() else {
            return Err(LayoutError::InvalidTree);
        };
        let command_base = self.commands.len() - fragment.commands.len();
        let hit_base = geometry.hits.len();
        let transform_base = geometry.transforms.len();
        let clip_base = geometry.rounded_clips.len();
        geometry
            .hits
            .try_reserve(fragment.hits.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        geometry
            .transforms
            .try_reserve(fragment.transforms.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        geometry
            .rounded_clips
            .try_reserve(fragment.rounded_clips.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        geometry
            .scroll_extents
            .try_reserve(fragment.scroll_extents.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        geometry
            .scroll_regions
            .try_reserve(fragment.scroll_regions.len())
            .map_err(|_| LayoutError::CommandLimit)?;
        geometry
            .control_text_runs
            .try_reserve(fragment.control_text_runs.len())
            .map_err(|_| LayoutError::CommandLimit)?;

        for hit in fragment.hits.iter() {
            geometry.hits.push(*hit);
        }
        for transform in fragment.transforms.iter() {
            let mut transform = transform.clone();
            transform.hits = rebase_range(transform.hits, hit_base)?;
            geometry.transforms.push(transform);
        }
        for clip in fragment.rounded_clips.iter() {
            let mut clip = clip.clone();
            clip.first_transform = transform_base
                .checked_add(clip.first_transform)
                .ok_or(LayoutError::InvalidTree)?;
            clip.hits = rebase_range(clip.hits, hit_base)?;
            geometry.rounded_clips.push(clip);
        }
        for extent in fragment.scroll_extents.iter() {
            geometry.scroll_extents.push(*extent);
        }
        for region in fragment.scroll_regions.iter() {
            let mut region = region.clone();
            region.commands = rebase_range(region.commands, command_base)?;
            region.hits = rebase_range(region.hits, hit_base)?;
            region.transforms = rebase_range(region.transforms, transform_base)?;
            region.clips = rebase_range(region.clips, clip_base)?;
            geometry.scroll_regions.push(region);
        }
        for run in fragment.control_text_runs.iter() {
            let mut run = run.clone();
            run.hit_index = run
                .hit_index
                .map(|index| hit_base.checked_add(index).ok_or(LayoutError::InvalidTree))
                .transpose()?;
            run.command_index = run
                .command_index
                .map(|index| {
                    command_base
                        .checked_add(index)
                        .ok_or(LayoutError::InvalidTree)
                })
                .transpose()?;
            geometry.control_text_runs.push(run);
        }
        self.collapsed_bottom = fragment.collapsed_bottom;
        self.was_through = fragment.was_through;
        Ok(())
    }

    fn capture_retained_fragment(
        &self,
        key: RetainedFragmentKey,
        command_start: usize,
        starts: (usize, usize, usize, usize, usize, usize),
        advance: f32,
    ) -> Option<RetainedFragment> {
        let geometry = self.geometry.as_deref()?;
        let (hit_start, transform_start, clip_start, extent_start, region_start, run_start) =
            starts;
        let commands = clone_arc_slice(self.commands.get(command_start..)?)?;
        let hits = clone_arc_slice(geometry.hits.get(hit_start..)?)?;
        let mut transforms = clone_vec(geometry.transforms.get(transform_start..)?)?;
        let mut rounded_clips = clone_vec(geometry.rounded_clips.get(clip_start..)?)?;
        let scroll_extents = clone_arc_slice(geometry.scroll_extents.get(extent_start..)?)?;
        let mut scroll_regions = clone_vec(geometry.scroll_regions.get(region_start..)?)?;
        let mut control_text_runs = clone_vec(geometry.control_text_runs.get(run_start..)?)?;

        for transform in &mut transforms {
            transform.hits = relative_range(transform.hits.clone(), hit_start)?;
        }
        for clip in &mut rounded_clips {
            clip.hits = relative_range(clip.hits.clone(), hit_start)?;
            clip.first_transform = clip.first_transform.checked_sub(transform_start)?;
        }
        for region in &mut scroll_regions {
            region.commands = relative_range(region.commands.clone(), command_start)?;
            region.hits = relative_range(region.hits.clone(), hit_start)?;
            region.transforms = relative_range(region.transforms.clone(), transform_start)?;
            region.clips = relative_range(region.clips.clone(), clip_start)?;
        }
        for run in &mut control_text_runs {
            if let Some(index) = run.hit_index {
                run.hit_index = Some(index.checked_sub(hit_start)?);
            }
            if let Some(index) = run.command_index {
                run.command_index = Some(index.checked_sub(command_start)?);
            }
        }
        let transforms: Arc<[HitTransform]> = Arc::from(transforms.into_boxed_slice());
        let rounded_clips: Arc<[HitClip]> = Arc::from(rounded_clips.into_boxed_slice());
        let scroll_regions: Arc<[ScrollRegion]> = Arc::from(scroll_regions.into_boxed_slice());
        let control_text_runs: Arc<[ControlTextRun]> =
            Arc::from(control_text_runs.into_boxed_slice());

        let bytes = retained_fragment_bytes(
            &key,
            &commands,
            &hits,
            &transforms,
            &rounded_clips,
            &scroll_extents,
            &scroll_regions,
            &control_text_runs,
        )?;
        Some(RetainedFragment {
            key,
            advance,
            collapsed_bottom: self.collapsed_bottom,
            was_through: self.was_through,
            commands,
            hits,
            transforms,
            rounded_clips,
            scroll_extents,
            scroll_regions,
            control_text_runs,
            bytes,
            last_used: 0,
        })
    }

    fn box_for(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        let computed = self
            .opacity_style(id, parent_style, computed)?
            .map(|style| {
                let containing = if style.position == Position::Fixed {
                    self.fixed_containing_block.or(Some(self.viewport))
                } else if style.position == Position::Absolute {
                    self.containing_block.or(Some(self.viewport))
                } else {
                    None
                };
                style.resolve_percentages(
                    containing.map_or(available, |rect| rect.width),
                    containing.map_or(self.parent_height, |rect| Some(rect.height)),
                )
            });
        let key = self.retained_fragment_key(id, parent_style, &computed, x, y, available, depth);
        if let Some(fragment) = key
            .as_ref()
            .and_then(|key| self.retained_fragments.as_deref_mut()?.lookup(key))
        {
            self.replay_retained_fragment(&fragment)?;
            return Ok(fragment.advance);
        }

        let command_start = self.commands.len();
        let geometry_start = self.geometry.as_deref().map(|geometry| {
            (
                geometry.hits.len(),
                geometry.transforms.len(),
                geometry.rounded_clips.len(),
                geometry.scroll_extents.len(),
                geometry.scroll_regions.len(),
                geometry.control_text_runs.len(),
            )
        });
        let pending_before = self.pending_escapes.clone();
        let positioned_before = self.positioned_flow_paints.clone();
        let stacking_before = self.stacking_roots.clone();
        let next_paint_before = self.next_paint_order;
        let hazards_before = self.scroll_hazards;

        let advance = self.box_for_uncached(id, parent_style, computed, x, y, available, depth)?;
        if let (Some(key), Some(starts)) = (key, geometry_start) {
            let stable = self.next_paint_order == next_paint_before
                && self.scroll_hazards == hazards_before
                && self.pending_escapes == pending_before
                && self.positioned_flow_paints == positioned_before
                && self.stacking_roots == stacking_before
                && self.parent_height == key.parent_height
                && self.containing_block == key.containing_block
                && self.fixed_containing_block == key.fixed_containing_block
                && self.sticky_bounds == key.sticky_bounds
                && self.cull == key.cull
                && self.transform_depth == key.transform_depth
                && self.body_background_on_canvas == key.body_background_on_canvas
                && self.decorations == key.decorations;
            if stable {
                if let Some(fragment) =
                    self.capture_retained_fragment(key, command_start, starts, advance)
                {
                    if let Some(cache) = self.retained_fragments.as_deref_mut() {
                        cache.store(fragment);
                    }
                }
            }
        }
        Ok(advance)
    }

    fn box_for_uncached(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        mut computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        let relative_flow_order = computed
            .as_ref()
            .filter(|style| style.position == Position::Relative && style.z_index.is_none())
            .map(|_| self.next_paint_order());
        let saved = self.containing_block;
        let saved_fixed = self.fixed_containing_block;
        let saved_sticky = self.sticky_bounds;
        let saved_height = self.parent_height;
        let saved_decorations = self.decorations.len();
        let mut origin = (x, y);
        let mut available = available;
        let mut out_of_flow = false;
        let mut trailing = (None, None);
        if let Some(style) = computed.as_mut() {
            if style.empty_cells_hide
                && matches!(
                    self.document.kind(id),
                    Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "td" | "th")
                )
                && self
                    .document
                    .first_child(id)
                    .map_err(|_| LayoutError::InvalidTree)?
                    .is_none()
            {
                style.visibility_visible = false;
            }
            self.begin_decoration(style)?;
            out_of_flow = matches!(style.position, Position::Absolute | Position::Fixed);
            if out_of_flow || style.position == Position::Sticky {
                self.scroll_hazards += 1;
            }
            if !out_of_flow
                && style.float == Float::None
                && !matches!(parent_style.display, Display::Flex | Display::Grid)
                && matches!(
                    style.display,
                    Display::Block | Display::Flex | Display::Grid
                )
            {
                let left_auto = style.margin_auto[3];
                let right_auto = style.margin_auto[1];
                let left = if left_auto {
                    0.0
                } else {
                    style.margin_sides[3]
                };
                let right = if right_auto {
                    0.0
                } else {
                    style.margin_sides[1]
                };
                let edges = style.padding_sides[1]
                    + style.padding_sides[3]
                    + if style.border_solid {
                        2.0 * style.border_width
                    } else {
                        0.0
                    };
                let content = constrained_width(
                    style,
                    style
                        .width
                        .map_or((available - left - right - edges).max(0.0), |width| {
                            content_dimension(style, width)
                        }),
                );
                let free = available - edges - content - left - right;
                let (used_left, used_right) = if free >= 0.0 && (left_auto || right_auto) {
                    match (left_auto, right_auto) {
                        (true, true) => (free * 0.5, free * 0.5),
                        (true, false) => (free, right),
                        _ => (left, free),
                    }
                } else if parent_style.direction == Direction::Rtl {
                    (left + free, right)
                } else {
                    (left, right)
                };
                if style.margin_sides[3] != used_left {
                    style.margin_sides[3] = used_left;
                }
                if style.margin_sides[1] != used_right {
                    style.margin_sides[1] = used_right;
                }
            }
            if out_of_flow {
                let containing = if style.position == Position::Fixed {
                    saved_fixed.unwrap_or(self.viewport)
                } else {
                    saved.unwrap_or(self.viewport)
                };
                available = containing.width;
                if style.width.is_none() {
                    if let (Some(left), Some(right)) = (style.left, style.right) {
                        style.width = Some(specified_dimension(
                            style,
                            (containing.width - left - right - box_edges(style, true)).max(0.0),
                        ));
                    } else {
                        let (intrinsic, _) = self.intrinsic_size(id, style, depth + 1)?;
                        style.width = Some(specified_dimension(
                            style,
                            (intrinsic.min(containing.width) - box_edges(style, true)).max(0.0),
                        ));
                    }
                }
                if style.height.is_none() {
                    if let (Some(top), Some(bottom)) = (style.top, style.bottom) {
                        style.height = Some(specified_height(
                            style,
                            (containing.height - top - bottom - box_edges(style, false)).max(0.0),
                        ));
                    }
                }
                origin.0 = style.left.map_or(x, |left| containing.x + left);
                origin.1 = style.top.map_or(y, |top| containing.y + top);
                if style.left.is_none() {
                    trailing.0 = style
                        .right
                        .map(|right| containing.x + containing.width - right);
                    if let Some(edge) = trailing.0 {
                        origin.0 = edge
                            - content_dimension(style, style.width.unwrap_or(0.0))
                            - box_edges(style, true);
                    }
                }
                if style.top.is_none() {
                    trailing.1 = style
                        .bottom
                        .map(|bottom| containing.y + containing.height - bottom);
                    if let Some(edge) = trailing.1 {
                        let height = if let Some(height) = style.height {
                            content_height_dimension(style, height) + box_edges(style, false)
                        } else {
                            self.intrinsic_size(id, style, depth + 1)?.1
                        };
                        origin.1 = edge - height;
                    }
                }
            } else if style.position == Position::Relative {
                origin.0 += style.left.unwrap_or_else(|| -style.right.unwrap_or(0.0));
                origin.1 += style.top.unwrap_or_else(|| -style.bottom.unwrap_or(0.0));
            } else if style.position == Position::Sticky {
                let containing = self.sticky_bounds.unwrap_or(self.viewport);
                let width = style.width.map_or(available, |width| {
                    content_dimension(style, width) + box_edges(style, true)
                });
                let height = style.height.map_or_else(
                    || self.intrinsic_size(id, style, depth + 1).map(|size| size.1),
                    |height| Ok(content_height_dimension(style, height) + box_edges(style, false)),
                )?;
                if let Some(top) = style.top {
                    origin.1 = origin.1.max(containing.y + top);
                }
                if let Some(bottom) = style.bottom {
                    origin.1 = origin
                        .1
                        .min(containing.y + containing.height - bottom - height);
                }
                if let Some(left) = style.left {
                    origin.0 = origin.0.max(containing.x + left);
                }
                if let Some(right) = style.right {
                    origin.0 = origin
                        .0
                        .min(containing.x + containing.width - right - width);
                }
            }
            if style.position != Position::Static
                || style.transforms.is_some()
                || style.contain_layout
            {
                let border = if style.border_solid {
                    style.border_width
                } else {
                    0.0
                };
                self.containing_block = Some(Rect {
                    x: origin.0 + style.margin_sides[3] + border,
                    y: origin.1 + style.margin_sides[0] + border,
                    width: style.width.map_or(
                        available - style.margin_sides[1] - style.margin_sides[3] - 2.0 * border,
                        |width| {
                            content_dimension(style, width)
                                + style.padding_sides[1]
                                + style.padding_sides[3]
                        },
                    ),
                    height: style
                        .height
                        .map_or(0.0, |height| content_height_dimension(style, height))
                        + style.padding_sides[0]
                        + style.padding_sides[2],
                });
                if style.transforms.is_some() {
                    self.fixed_containing_block = self.containing_block;
                }
            }
            if overflow_scroll_container(&style) && style.height.is_some() {
                let border = if style.border_solid {
                    style.border_width
                } else {
                    0.0
                };
                self.sticky_bounds = Some(Rect {
                    x: origin.0 + border,
                    y: origin.1 + border,
                    width: style.width.map_or(
                        (available - style.margin_sides[1] - style.margin_sides[3]).max(0.0),
                        |width| {
                            content_dimension(style, width)
                                + style.padding_sides[1]
                                + style.padding_sides[3]
                        },
                    ),
                    height: content_height_dimension(style, style.height.unwrap())
                        + style.padding_sides[0]
                        + style.padding_sides[2],
                });
            }
            self.parent_height = style
                .height
                .map(|height| constrained_height(style, content_height_dimension(style, height)));
        }
        let first_command = self.commands.len();
        let first_hit = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        let transform_style = computed
            .as_ref()
            .filter(|style| style.transforms.is_some())
            .cloned();
        let transform_command = if transform_style.is_some() {
            let index = self.commands.len();
            self.push_command(Command::PushTransform(crate::paint::Affine::IDENTITY))?;
            self.transform_depth += 1;
            Some(index)
        } else {
            None
        };
        let layer = self.begin_opacity(computed.as_ref())?;
        let width = computed.as_ref().map_or(available, |style| {
            let border = if style.border_solid {
                2.0 * style.border_width
            } else {
                0.0
            };
            style.width.map_or(available, |width| {
                content_dimension(style, width)
                    + style.padding_sides[1]
                    + style.padding_sides[3]
                    + border
                    + style.margin_sides[1]
                    + style.margin_sides[3]
            })
        });
        let result = self.box_content(
            id,
            parent_style,
            computed,
            origin.0,
            origin.1,
            available,
            depth,
        );
        self.containing_block = saved;
        self.fixed_containing_block = saved_fixed;
        self.sticky_bounds = saved_sticky;
        self.parent_height = saved_height;
        self.decorations.truncate(saved_decorations);
        if transform_command.is_some() {
            self.transform_depth -= 1;
        }
        let result = result?;
        self.end_opacity(layer)?;
        if let (Some(index), Some(style)) = (transform_command, transform_style.as_ref()) {
            let fallback = Rect {
                x: origin.0 + style.margin_sides[3],
                y: origin.1 + style.margin_sides[0],
                width: (width - style.margin_sides[1] - style.margin_sides[3]).max(0.0),
                height: (result - style.margin_sides[0] - style.margin_sides[2]).max(0.0),
            };
            let rect = self
                .geometry
                .as_ref()
                .and_then(|g| g.hits.get(first_hit))
                .filter(|hit| hit.node == id)
                .map_or(fallback, |hit| hit.rect);
            let matrix = style
                .transform_matrix(rect)
                .ok_or(LayoutError::Css(css::CssError {
                    offset: 0,
                    message: "invalid computed transform",
                }))?;
            self.replace_command(index, Command::PushTransform(matrix))?;
            if self.commands.len() == index + 1 {
                self.pop_command();
            } else {
                self.push_command(Command::PopTransform)?;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if geometry.transforms.len() == MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                geometry
                    .transforms
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                let transform_hits = first_hit..geometry.hits.len();
                geometry.transforms.push(HitTransform {
                    hits: transform_hits.clone(),
                    matrix,
                });
                geometry.refresh_control_run_transforms(transform_hits);
            }
        }
        let dx = trailing.0.map_or(0.0, |edge| edge - origin.0 - width);
        let dy = trailing.1.map_or(0.0, |edge| edge - origin.1 - result);
        if dx != 0.0 || dy != 0.0 {
            for command in &mut self.commands[first_command..] {
                move_command(command, dx, dy);
            }
            if let Some(geometry) = self.geometry.as_mut() {
                geometry.move_hits(first_hit..geometry.hits.len(), dx, dy);
            }
        }
        if let Some(order) = relative_flow_order {
            let commands = first_command..self.commands.len();
            let hits = first_hit
                ..self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
            self.discard_positioned_flow_in(commands.clone(), hits.clone());
            if self.positioned_flow_paints.len() >= MAX_DISPLAY_COMMANDS {
                return Err(LayoutError::CommandLimit);
            }
            self.positioned_flow_paints
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            self.positioned_flow_paints.push(PositionedFlowPaint {
                order,
                commands,
                hits,
            });
        }
        Ok(if out_of_flow { 0.0 } else { result })
    }

    fn form_control_text<N: AsRef<str>>(
        &self,
        id: NodeId,
        name: &str,
        attributes: &[(N, String)],
    ) -> Result<Option<FormText>, LayoutError> {
        let attr = |key: &str| {
            attributes
                .iter()
                .find(|(attribute, _)| attribute.as_ref() == key)
                .map(|(_, value)| value.as_str())
        };
        let (mut value, multiline, password) = if name == "textarea" {
            let mut text = String::new();
            if let Some(value) = attr("value") {
                append_bounded_control_text(&mut text, value)?;
            } else {
                let mut child = self
                    .document
                    .first_child(id)
                    .map_err(|_| LayoutError::InvalidTree)?;
                while let Some(current) = child {
                    if let NodeKind::Text(value) = self
                        .document
                        .kind(current)
                        .map_err(|_| LayoutError::InvalidTree)?
                    {
                        append_bounded_control_text(&mut text, value)?;
                    }
                    child = self
                        .document
                        .next_sibling(current)
                        .map_err(|_| LayoutError::InvalidTree)?;
                }
            }
            (text, true, false)
        } else if name == "input" {
            let input_type = attr("type").unwrap_or("text");
            if css::input_type_is_nontext(input_type) {
                return Ok(None);
            }
            let mut text = String::new();
            append_bounded_control_text(&mut text, attr("value").unwrap_or(""))?;
            (text, false, input_type.eq_ignore_ascii_case("password"))
        } else {
            return Ok(None);
        };

        let mut placeholder = false;
        if value.is_empty() {
            if let Some(placeholder_text) = attr("placeholder") {
                append_bounded_control_text(&mut value, placeholder_text)?;
                placeholder = true;
            }
        }
        Ok(Some(FormText {
            value,
            placeholder,
            password,
            multiline,
        }))
    }

    fn paint_form_control_text(
        &mut self,
        id: NodeId,
        hit_index: Option<usize>,
        control: FormText,
        style: &Style,
        left: f32,
        top: f32,
        width: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        let line_height = self.line_height(style).max(1.0);
        let FormText {
            mut value,
            placeholder,
            password,
            multiline,
        } = control;
        let control_source_ranges = if password && !placeholder {
            let ranges = control_grapheme_ranges(&value)?;
            let display_bytes = ranges
                .len()
                .checked_mul('\u{2022}'.len_utf8())
                .ok_or(LayoutError::CommandLimit)?;
            if display_bytes > MAX_CONTROL_TEXT_BYTES {
                return Err(LayoutError::CommandLimit);
            }
            let mut masked = String::new();
            masked
                .try_reserve(display_bytes)
                .map_err(|_| LayoutError::CommandLimit)?;
            masked.extend(core::iter::repeat_n('\u{2022}', ranges.len()));
            value = masked;
            Some(ranges)
        } else {
            None
        };
        if value.is_empty() {
            let line_rect = Rect {
                x: left,
                y: top,
                width,
                height: line_height,
            };
            if style.visibility_visible
                && self.geometry.is_some()
                && (self.transform_depth > 0 || line_rect.intersection(self.cull).is_some())
            {
                let y = if multiline {
                    top
                } else {
                    top + ((style
                        .height
                        .map(|height| content_height_dimension(style, height))
                        .unwrap_or(line_height)
                        - line_height)
                        * 0.5)
                        .max(0.0)
                };
                let baseline_y = y + self.baseline(style);
                if let Some(geometry) = self.geometry.as_mut() {
                    if geometry.control_text_runs.len() >= MAX_DISPLAY_COMMANDS {
                        return Err(LayoutError::CommandLimit);
                    }
                    geometry
                        .control_text_runs
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    geometry.control_text_runs.push(ControlTextRun {
                        node: id,
                        range: 0..0,
                        display_range: 0..0,
                        hit_index,
                        command_index: None,
                        x: left,
                        y,
                        width: 0.0,
                        height: line_height,
                        baseline_y,
                        rtl: style.direction == Direction::Rtl,
                        font_size: style.font_size,
                        font: style.font.clone(),
                        placeholder,
                        password,
                        transform: crate::paint::Affine::IDENTITY,
                    });
                }
            }
            return Ok(if multiline { line_height } else { 0.0 });
        }
        let mut paint_style = style.clone();
        if placeholder {
            paint_style.color = Rgba {
                r: 117,
                g: 117,
                b: 117,
                a: 255,
            };
        }
        if !multiline {
            paint_style.white_space = WhiteSpace::NoWrap;
        }
        let content_height = style
            .height
            .map(|height| content_height_dimension(style, height))
            .unwrap_or(line_height)
            .max(line_height);
        let clip_height = if multiline {
            if style.height.is_some() {
                content_height
            } else {
                (self.cull_bottom() - top).max(line_height)
            }
        } else {
            content_height
        };
        let clip = Rect {
            x: left,
            y: top,
            width: width.max(0.0),
            height: clip_height,
        };
        if !style.visibility_visible
            || width <= 0.0
            || (self.transform_depth == 0 && clip.intersection(self.cull).is_none())
        {
            return Ok(line_height);
        }
        self.push_command(Command::PushClip(clip))?;
        let text_top = if multiline {
            top
        } else {
            top + ((content_height - line_height) * 0.5).max(0.0)
        };
        let mut cursor = text_top;
        let mut advance = 0.0;
        let mut height = 0.0;
        let mut line = self.line_start();
        let mut trailing = 0.0;
        let mut paragraph = InlineParagraph {
            text: value,
            control_node: Some(id),
            control_hit_index: hit_index,
            control_placeholder: placeholder,
            control_password: password,
            control_source_ranges,
            ..InlineParagraph::default()
        };
        paragraph
            .spans
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        paragraph.spans.push(InlineTextSpan {
            range: 0..paragraph.text.len(),
            style_node: id,
            style: paint_style.clone(),
            frames: Vec::new(),
        });
        self.flow_inline_paragraph(
            &mut paragraph,
            &paint_style,
            left,
            width,
            &mut cursor,
            &mut advance,
            &mut height,
            &[],
            &mut line,
            &mut trailing,
            depth,
        )?;
        let flow_height = if multiline {
            cursor - text_top + height.max(line_height)
        } else {
            line_height
        };
        self.push_command(Command::PopClip)?;
        Ok(flow_height.max(line_height))
    }

    fn box_content(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let positioned_flow_start = self.positioned_flow_paints.len();
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            if value.is_empty() {
                return Ok(0.0);
            }
            let size = parent_style.font_size;
            let line_height = self.line_height(parent_style);
            if self.transform_depth == 0
                && (x >= self.cull_right()
                    || y >= self.cull_bottom()
                    || (line_height > 0.0 && y + line_height <= self.cull.y))
            {
                return Ok(line_height);
            }
            let shaped = self
                .text
                .shape_styled(
                    value,
                    size,
                    parent_style.direction == Direction::Rtl,
                    &parent_style.font,
                )
                .map_err(|_| LayoutError::Text)?;
            if parent_style.visibility_visible {
                self.push_command(Command::GlyphRun {
                    origin_x: x,
                    baseline_y: y + self.baseline(parent_style),
                    size,
                    color: parent_style.color,
                    glyphs: shaped.glyphs,
                })?;
            }
            return Ok(line_height);
        }
        let NodeKind::Element {
            name,
            attributes,
            namespace,
        } = kind
        else {
            return Ok(0.0);
        };
        let svg_root = *namespace == Namespace::Svg && crate::svg::local_name(name) == "svg";
        if *namespace != Namespace::Html && !svg_root {
            return Ok(0.0);
        }
        let tag = crate::svg::local_name(name);
        if *namespace == Namespace::Html
            && matches!(
                tag,
                "head" | "style" | "script" | "meta" | "link" | "template"
            )
        {
            return Ok(0.0);
        }
        let mut style = match computed {
            Some(style) => style,
            None => self
                .computed_style(id, Some(parent_style))
                .map_err(LayoutError::Css)?,
        };
        if style.display == Display::None {
            return Ok(0.0);
        }
        let generated_before =
            self.virtual_generated_child(id, &style, css::PseudoElement::Before, available)?;
        let generated_after =
            self.virtual_generated_child(id, &style, css::PseudoElement::After, available)?;
        let is_form_control = matches!(tag, "input" | "textarea");
        if (generated_before.is_some() || generated_after.is_some())
            && (svg_root
                || is_form_control
                || matches!(style.display, Display::Table)
                || matches!(tag, "img" | "canvas" | "video"))
        {
            return Err(LayoutError::UnsupportedGeneratedContent);
        }
        if svg_root {
            return self.paint_svg_root(id, attributes, &style, x, y, available, depth + 1);
        }
        let form_text = if is_form_control {
            self.form_control_text(id, tag, attributes)?
        } else {
            None
        };
        let has_fixed_background = has_layer_attachment(&style, css::BackgroundAttachment::Fixed);
        let has_local_background = overflow_scroll_container(&style)
            && has_layer_attachment(&style, css::BackgroundAttachment::Local);
        if has_fixed_background {
            // A scrolling ancestor must relayout so fixed layers keep their
            // viewport positioning area while the element's clip moves.
            self.scroll_hazards += 1;
        }
        if style.display == Display::Table {
            return self.table(id, &style, x, y, available, depth);
        }
        // CSS Sizing §6.1: aspect-ratio derives the auto dimension from the
        // definite one.
        if let Some(ratio) = style.aspect_ratio {
            match (style.width, style.height) {
                (Some(width), None) => {
                    let height = content_dimension(&style, width) / ratio;
                    style.height = Some(specified_height(&style, height));
                }
                (None, Some(height)) => {
                    let width = content_height_dimension(&style, height) * ratio;
                    style.width = Some(specified_dimension(&style, width));
                }
                _ => {}
            }
        }
        let hit = self.begin_hit(id)?;
        let border_sides = border_widths(&style);
        let [border_top, border_right, border_bottom, border_left] = border_sides;
        let border_width = border_sides.into_iter().fold(0.0f32, f32::max);
        let [margin_top, margin_right, margin_bottom, margin_left] = style.margin_sides;
        let [padding_top, padding_right, padding_bottom, padding_left] = style.padding_sides;
        let outer_x = x + margin_left;
        let outer_y = y + margin_top;
        let content_width = constrained_width(
            &style,
            style
                .width
                .map(|width| content_dimension(&style, width))
                .unwrap_or(
                    (available
                        - margin_left
                        - margin_right
                        - padding_left
                        - padding_right
                        - border_left
                        - border_right)
                        .max(0.0),
                )
                .max(0.0),
        );
        let box_width = content_width + padding_left + padding_right + border_left + border_right;
        if tag == "img" || tag == "canvas" || tag == "video" {
            let dimension = |key: &str| {
                if tag == "canvas" {
                    let raw = crate::svg::attribute(attributes, key);
                    return Some(
                        canvas_dimension(raw, if key == "width" { 300 } else { 150 }) as f32,
                    );
                }
                crate::svg::attribute(attributes, key)
                    .and_then(|value| value.parse::<f32>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
            };
            let specified_width = style
                .width
                .or_else(|| dimension("width"))
                .map(|width| content_dimension(&style, width));
            let specified_height = style
                .height
                .or_else(|| dimension("height"))
                .map(|height| content_height_dimension(&style, height));
            if self.transform_depth == 0 && outer_y >= self.cull_bottom() {
                if let Some(height) = specified_height {
                    return Ok(height
                        + padding_top
                        + padding_bottom
                        + 2.0 * border_width
                        + margin_top
                        + margin_bottom);
                }
            }
            let source = crate::svg::attribute(attributes, "src");
            let resolved = if tag == "canvas" || tag == "video" {
                self.images.and_then(|images| images.resolve_node(id))
            } else {
                match (source, self.images) {
                    (source, Some(images)) => {
                        resolve_element_image(images, id, source, self.rules.document_base_url())
                    }
                    (Some(_), None) => Some(ImageState::Failed),
                    _ => None,
                }
            };
            let image = match resolved {
                Some(ImageState::Ready(image)) if image.is_valid() => Some(image),
                Some(ImageState::Ready(_)) => return Err(LayoutError::ImageFailed),
                Some(ImageState::Pending) => return Err(LayoutError::ImagePending),
                Some(ImageState::Failed) => return Err(LayoutError::ImageFailed),
                None => None,
            };
            if let Some(image) = image {
                let (image_width, image_height) = match (specified_width, specified_height) {
                    (Some(width), Some(height)) => (width, height),
                    (Some(width), None) => {
                        (width, width * image.height as f32 / image.width as f32)
                    }
                    (None, Some(height)) => {
                        (height * image.width as f32 / image.height as f32, height)
                    }
                    (None, None) => (image.width as f32, image.height as f32),
                };
                let rect = Rect {
                    x: outer_x + border_width + padding_left,
                    y: outer_y + border_width + padding_top,
                    width: image_width,
                    height: image_height,
                };
                let outer_rect = Rect {
                    x: outer_x,
                    y: outer_y,
                    width: image_width + padding_left + padding_right + 2.0 * border_width,
                    height: image_height + padding_top + padding_bottom + 2.0 * border_width,
                };
                self.finish_hit(hit, outer_rect);
                if style.visibility_visible
                    && has_background(&style)
                    && (self.transform_depth > 0 || outer_rect.intersection(self.cull).is_some())
                {
                    self.push_background(outer_rect, &style)?;
                }
                if style.visibility_visible
                    && self.suppressed_border_node != Some(id)
                    && border_width > 0.0
                    && (self.transform_depth > 0 || outer_rect.intersection(self.cull).is_some())
                {
                    self.push_command(border(outer_rect, &style))?;
                }
                if style.visibility_visible
                    && (self.transform_depth > 0 || rect.intersection(self.cull).is_some())
                {
                    self.push_command(Command::Image { rect, image })?;
                }
                return Ok(image_height
                    + padding_top
                    + padding_bottom
                    + 2.0 * border_width
                    + margin_top
                    + margin_bottom);
            }
        }
        // Stacking-context roots paint their negative-z descendants above
        // their own background; plain containers let them escape below it
        // (CSS 2.1 Appendix E, layers 2-3).
        let self_is_stacking_context = (style.position != Position::Static
            && style.z_index.is_some())
            || matches!(style.position, Position::Fixed | Position::Sticky)
            || style.transforms.is_some()
            || style.opacity < 1.0
            || style.contain_paint
            || style.contain_layout;
        let mut negatives_laid: Vec<NodeId> = Vec::new();
        if !self_is_stacking_context && self.rules.has_z_index {
            self.pre_layout_negative_z(
                id,
                &style,
                outer_x,
                outer_y,
                content_width,
                depth,
                &mut negatives_laid,
                true,
            )?;
        }
        let background_index = if !style.visibility_visible
            || !has_background(&style)
            || (self.transform_depth == 0
                && (outer_y >= self.cull_bottom() || outer_x >= self.cull_right()))
            || (name == "body" && self.body_background_on_canvas)
        {
            None
        } else {
            let index = self.commands.len() + background_paints(&style) - 1;
            self.push_background(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: 0.0,
                },
                &style,
            )?;
            Some(index)
        };
        let border_index = if self.suppressed_border_node != Some(id)
            && style.visibility_visible
            && !has_nonuniform_border(&style)
            && border_width > 0.0
            && style.border_color.a > 0
            && (self.transform_depth > 0
                || (outer_y < self.cull_bottom() && outer_x < self.cull_right()))
        {
            let index = self.commands.len();
            self.push_command(border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: 0.0,
                },
                &style,
            ))?;
            Some(index)
        } else {
            None
        };
        let scroll_x_container = style.overflow_x.scroll_container();
        let scroll_y_container = style.overflow_y.scroll_container();
        let scroll_container = scroll_x_container || scroll_y_container;
        let clip_x = overflow_clips_x(&style);
        let clip_y = overflow_clips_y(&style);
        let scroll = if scroll_container {
            self.scrolls
                .iter()
                .find(|scroll| scroll.node == id)
                .copied()
        } else {
            None
        };
        let (scroll_x, scroll_y) = scroll.map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y));
        let clip_index = if clip_x || clip_y {
            let index = self.commands.len();
            self.push_command(Command::PushClip(Rect {
                x: outer_x + border_left,
                y: outer_y + border_top,
                width: content_width + padding_left + padding_right,
                height: 0.0,
            }))?;
            Some(index)
        } else {
            None
        };
        let saved_cull = self.cull;
        let mut scroll_window = None;
        if scroll_container && self.geometry.is_some() {
            let clip_height = style
                .height
                .map(|height| constrained_height(&style, content_height_dimension(&style, height)))
                .or_else(|| {
                    style
                        .max_height
                        .map(|height| content_height_dimension(&style, height))
                })
                .filter(|_| {
                    style.height_intrinsic.is_none()
                        && style.min_height_intrinsic.is_none()
                        && style.max_height_intrinsic.is_none()
                })
                .map(|height| height + padding_top + padding_bottom);
            let clip = Rect {
                x: outer_x + border_left,
                y: outer_y + border_top,
                width: content_width + padding_left + padding_right,
                height: clip_height.unwrap_or(f32::INFINITY),
            };
            if let Some(visible) = clip.intersection(self.cull) {
                let grow_y = if scroll_y_container && clip_height.is_some() {
                    visible.height
                } else {
                    0.0
                };
                let (left, right) = if scroll_x_container {
                    (
                        self.cull.x.min(visible.x - visible.width),
                        self.cull_right().max(visible.x + 2.0 * visible.width),
                    )
                } else {
                    (self.cull.x, self.cull_right())
                };
                let (top, bottom) = if scroll_y_container && clip_height.is_some() {
                    (
                        self.cull.y.min(visible.y - grow_y),
                        self.cull_bottom().max(visible.y + visible.height + grow_y),
                    )
                } else {
                    (self.cull.y, self.cull_bottom())
                };
                self.cull = Rect {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                };
                let window = (
                    if scroll_x_container {
                        (visible.x - left).min(right - visible.x - visible.width)
                    } else {
                        0.0
                    },
                    if scroll_y_container {
                        (visible.y - top).min(bottom - visible.y - visible.height)
                    } else {
                        0.0
                    },
                );
                if (window.0 > 0.0 || window.1 > 0.0) && !has_local_background {
                    scroll_window = Some(((scroll_x, scroll_y), window, self.scroll_hazards));
                }
            }
        }
        let scroll_transforms = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.transforms.len());
        let scroll_clips = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.rounded_clips.len());
        let mut sc_root = None;
        if self_is_stacking_context && self.rules.has_z_index {
            let insert = (
                self.commands.len(),
                self.geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len()),
            );
            self.stacking_roots.push(insert);
            sc_root = Some(insert);
            self.pre_layout_negative_z(
                id,
                &style,
                outer_x,
                outer_y,
                content_width,
                depth,
                &mut negatives_laid,
                false,
            )?;
        }
        let first_child_hit = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        let content_x = outer_x + border_left + padding_left - scroll_x;
        let mut cursor =
            outer_y + border_top + padding_top + style.table_cell_content_offset - scroll_y;
        let vertical_block_flow = matches!(
            style.writing_mode,
            css::WritingMode::VerticalRl | css::WritingMode::VerticalLr
        );
        let mut vertical_block_cursor = if style.writing_mode == css::WritingMode::VerticalRl {
            content_x + content_width
        } else {
            content_x
        };
        let mut vertical_content_extent = 0.0f32;
        let mut content_extent_width = content_width;
        let mut inline_width = 0.0;
        let mut inline_height: f32 = 0.0;
        let mut line_start = self.line_start();
        let mut trailing_space = 0.0;
        let mut floats: Vec<(Rect, Float)> = Vec::new();
        let mut positioned: Vec<(NodeId, Style, f32, f32, u64)> = Vec::new();
        let column_count = style.column_count.unwrap_or(1).clamp(1, 64);
        let column_gap = resolved_gap(
            style.column_gap.unwrap_or(style.font_size),
            style.column_gap_fraction,
            Some(content_width),
        );
        let column_width = if column_count > 1 {
            (content_width - column_gap * (column_count - 1) as f32).max(0.0) / column_count as f32
        } else {
            content_width
        };
        let mut multicol_items = 0usize;
        let mut multicol_first_height = None;
        if column_count > 1 {
            let mut scan = self
                .document
                .composed_children_iter(id)
                .map_err(|_| LayoutError::InvalidTree)?;
            while let Some(node) = scan.next().map_err(|_| LayoutError::InvalidTree)? {
                if !matches!(
                    self.document.kind(node),
                    Ok(NodeKind::Element {
                        namespace: Namespace::Html,
                        ..
                    })
                ) {
                    continue;
                }
                let child = self
                    .computed_style(node, Some(&style))
                    .map_err(LayoutError::Css)?;
                if child.display != Display::None
                    && !matches!(child.position, Position::Absolute | Position::Fixed)
                    && child.display != Display::Inline
                {
                    multicol_items += 1;
                    if multicol_first_height.is_none() {
                        let edges = child.padding_sides[0]
                            + child.padding_sides[2]
                            + if child.border_solid {
                                2.0 * child.border_width
                            } else {
                                0.0
                            };
                        multicol_first_height = Some(
                            child.height.unwrap_or_else(|| self.line_height(&child))
                                + edges
                                + child.margin_sides[0]
                                + child.margin_sides[2],
                        );
                    }
                }
            }
        }
        let mut multicol_per_column = multicol_items.div_ceil(column_count.max(1)).max(1);
        if style.column_fill_auto && column_count > 1 && style.height.is_some() {
            if let Some(first) = multicol_first_height.filter(|first| *first > 0.0) {
                multicol_per_column = ((style.height.unwrap() / first).floor() as usize).max(1);
            }
        }
        let column_start_y = cursor;
        let mut multicol_cursors = if column_count > 1 {
            alloc::vec![column_start_y; column_count]
        } else {
            Vec::new()
        };
        let mut multicol_item_index = 0usize;
        let mut previous_multicol_column = None;
        let mut previous_margin: f32 = 0.0;
        let mut block_children: Vec<BlockChild> = Vec::new();
        let mut raised_flow = Vec::new();
        // In-flow content that interrupts a margin collapse chain: whitespace
        // only text and out-of-flow boxes do not interrupt (CSS 2.1 §8.3.1).
        let mut non_block_since = false;
        let mut last_was_block = false;
        let mut saw_non_block = false;
        let virtual_children_are_flex_grid = matches!(style.display, Display::Flex | Display::Grid);
        let mut generated_before_pending =
            generated_before.is_some() && !virtual_children_are_flex_grid;
        let mut generated_after_pending =
            generated_after.is_some() && !virtual_children_are_flex_grid;
        if let Some(control) = form_text {
            cursor += self.paint_form_control_text(
                id,
                hit,
                control,
                &style,
                content_x,
                cursor,
                content_width,
                depth + 1,
            )?;
        }
        let mut children: Option<crate::shadow::ComposedChildren<'_>> = if is_form_control {
            None
        } else if style.display == Display::Grid {
            let (extent_width, height) =
                self.grid_children(id, &style, content_x, cursor, content_width, depth)?;
            cursor += height;
            content_extent_width = content_extent_width.max(extent_width);
            None
        } else if style.display == Display::Flex {
            let (extent_width, height) =
                self.flex_children(id, &style, content_x, cursor, content_width, depth)?;
            cursor += height;
            content_extent_width = content_extent_width.max(extent_width);
            None
        } else {
            Some(
                self.document
                    .composed_children_iter(id)
                    .map_err(|_| LayoutError::InvalidTree)?,
            )
        };
        let mut pending_child = None;
        loop {
            let current = match pending_child.take() {
                Some(node) => Some(node),
                None => match &mut children {
                    Some(walk) => walk.next().map_err(|_| LayoutError::InvalidTree)?,
                    None => None,
                },
            };
            let Some(current) = current else {
                break;
            };
            let kind = self
                .document
                .kind(current)
                .map_err(|_| LayoutError::InvalidTree)?;
            let computed = if matches!(kind, NodeKind::Element { .. }) {
                Some(
                    self.computed_style(current, Some(&style))
                        .map_err(LayoutError::Css)
                        .map(|child| {
                            if matches!(child.position, Position::Absolute | Position::Fixed) {
                                child
                            } else {
                                child.resolve_percentages(content_width, self.parent_height)
                            }
                        })?,
                )
            } else {
                None
            };
            if generated_before_pending {
                let before = generated_before.as_ref().ok_or(LayoutError::InvalidTree)?;
                if before.style.display != Display::Inline {
                    if vertical_block_flow
                        || column_count > 1
                        || before.style.display != Display::Block
                    {
                        return Err(LayoutError::UnsupportedGeneratedContent);
                    }
                    self.flow_virtual_generated_block_child(
                        before,
                        &style,
                        content_x,
                        content_width,
                        &mut cursor,
                        &mut inline_width,
                        &mut inline_height,
                        &floats,
                        &mut line_start,
                        &mut trailing_space,
                        &mut previous_margin,
                        &mut block_children,
                        &mut non_block_since,
                        &mut last_was_block,
                        &mut content_extent_width,
                        depth + 1,
                    )?;
                    generated_before_pending = false;
                }
            }
            if self.paragraph_inline_eligible(
                current,
                &style,
                computed.as_ref(),
                content_width,
                depth + 1,
            )? {
                let mut paragraph = InlineParagraph {
                    has_preceding_content: inline_width > 0.0,
                    ..InlineParagraph::default()
                };
                let mut frame_path = Vec::new();
                if generated_before_pending {
                    let before = generated_before.as_ref().ok_or(LayoutError::InvalidTree)?;
                    self.append_virtual_generated_child(
                        before,
                        &style,
                        &mut paragraph,
                        &mut frame_path,
                    )?;
                    generated_before_pending = false;
                }
                self.collect_inline_paragraph_node(
                    current,
                    id,
                    &style,
                    computed.clone(),
                    content_width,
                    &mut paragraph,
                    &mut frame_path,
                    depth + 1,
                )?;
                let mut reached_end = false;
                loop {
                    let next = match &mut children {
                        Some(walk) => walk.next().map_err(|_| LayoutError::InvalidTree)?,
                        None => None,
                    };
                    let Some(next) = next else {
                        reached_end = true;
                        break;
                    };
                    let next_kind = self
                        .document
                        .kind(next)
                        .map_err(|_| LayoutError::InvalidTree)?;
                    let next_style = if matches!(next_kind, NodeKind::Element { .. }) {
                        Some(
                            self.computed_style(next, Some(&style))
                                .map_err(LayoutError::Css)
                                .map(|child| {
                                    if matches!(
                                        child.position,
                                        Position::Absolute | Position::Fixed
                                    ) {
                                        child
                                    } else {
                                        child.resolve_percentages(content_width, self.parent_height)
                                    }
                                })?,
                        )
                    } else {
                        None
                    };
                    if !self.paragraph_inline_eligible(
                        next,
                        &style,
                        next_style.as_ref(),
                        content_width,
                        depth + 1,
                    )? {
                        pending_child = Some(next);
                        break;
                    }
                    self.collect_inline_paragraph_node(
                        next,
                        id,
                        &style,
                        next_style,
                        content_width,
                        &mut paragraph,
                        &mut frame_path,
                        depth + 1,
                    )?;
                }
                if reached_end
                    && generated_after_pending
                    && generated_after
                        .as_ref()
                        .is_some_and(|after| after.style.display == Display::Inline)
                {
                    let after = generated_after.as_ref().ok_or(LayoutError::InvalidTree)?;
                    self.append_virtual_generated_child(
                        after,
                        &style,
                        &mut paragraph,
                        &mut frame_path,
                    )?;
                    generated_after_pending = false;
                }
                let has_inline_content = !paragraph.frames.is_empty()
                    || !paragraph.atoms.is_empty()
                    || paragraph.hard_breaks.len() > 0
                    || paragraph.text.chars().any(|character| {
                        !character.is_whitespace()
                            && !matches!(character, '\u{200b}' | '\u{2029}' | '\u{fffc}')
                    });
                self.flow_inline_paragraph(
                    &mut paragraph,
                    &style,
                    content_x,
                    content_width,
                    &mut cursor,
                    &mut inline_width,
                    &mut inline_height,
                    &floats,
                    &mut line_start,
                    &mut trailing_space,
                    depth + 1,
                )?;
                if has_inline_content {
                    non_block_since = true;
                    last_was_block = false;
                    saw_non_block = true;
                }
                content_extent_width = content_extent_width.max(inline_width);
                continue;
            }
            if generated_before_pending {
                let before = generated_before.as_ref().ok_or(LayoutError::InvalidTree)?;
                if before.style.display == Display::Inline {
                    if self.flow_virtual_generated_child(
                        before,
                        &style,
                        content_x,
                        content_width,
                        &mut cursor,
                        &mut inline_width,
                        &mut inline_height,
                        &floats,
                        &mut line_start,
                        &mut trailing_space,
                        depth + 1,
                    )? {
                        non_block_since = true;
                        last_was_block = false;
                        saw_non_block = true;
                    }
                } else {
                    if vertical_block_flow
                        || column_count > 1
                        || before.style.display != Display::Block
                    {
                        return Err(LayoutError::UnsupportedGeneratedContent);
                    }
                    self.flow_virtual_generated_block_child(
                        before,
                        &style,
                        content_x,
                        content_width,
                        &mut cursor,
                        &mut inline_width,
                        &mut inline_height,
                        &floats,
                        &mut line_start,
                        &mut trailing_space,
                        &mut previous_margin,
                        &mut block_children,
                        &mut non_block_since,
                        &mut last_was_block,
                        &mut content_extent_width,
                        depth + 1,
                    )?;
                }
                generated_before_pending = false;
            }
            let raised_count = raised_flow.len();
            if let Some(child_style) = &computed {
                // Geometry remains in flow; its paint chunk is raised after
                // margin collapse and line alignment have finished.
                if !matches!(child_style.position, Position::Absolute | Position::Fixed)
                    && (child_style.position != Position::Static
                        || child_style.transforms.is_some()
                        || child_style.opacity < 1.0
                        || child_style.contain_paint
                        || child_style.contain_layout)
                {
                    raised_flow.push((
                        self.commands.len()..self.commands.len(),
                        self.geometry.as_ref().map_or(0, |g| g.hits.len())
                            ..self.geometry.as_ref().map_or(0, |g| g.hits.len()),
                    ));
                }
                if child_style.display != Display::None {
                    content_extent_width = content_extent_width.max(
                        child_style
                            .width
                            .map(|width| content_dimension(child_style, width))
                            .unwrap_or(0.0)
                            + 2.0
                                * (child_style.padding
                                    + child_style.margin
                                    + if child_style.border_solid {
                                        child_style.border_width
                                    } else {
                                        0.0
                                    }),
                    );
                }
            }
            if computed
                .as_ref()
                .is_some_and(|style| style.display == Display::None)
                || matches!(
                    kind,
                    NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. }
                )
                || (matches!(kind, NodeKind::Element { namespace, .. } if *namespace != Namespace::Html)
                    && !is_svg_root_element(kind))
                || matches!(kind, NodeKind::Element { name, namespace: Namespace::Html, .. } if matches!(crate::svg::local_name(name), "head" | "style" | "script" | "meta" | "link" | "template" | "title" | "base"))
            {
            } else if computed
                .as_ref()
                .is_some_and(|s| matches!(s.position, Position::Absolute | Position::Fixed))
            {
                if positioned.len() == 4096 {
                    return Err(LayoutError::CommandLimit);
                }
                positioned
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                positioned.push((
                    current,
                    computed.unwrap(),
                    content_x + inline_width,
                    cursor,
                    self.next_paint_order(),
                ));
            } else if computed.as_ref().is_some_and(|s| s.float != Float::None) {
                let mut floated = computed.unwrap();
                let (intrinsic, _) = self.intrinsic_size(current, &floated, depth + 1)?;
                let width = floated.width.unwrap_or_else(|| {
                    specified_dimension(
                        &floated,
                        (intrinsic.min(content_width) - box_edges(&floated, true)).max(0.0),
                    )
                });
                floated.width = Some(width);
                let outer_width = content_dimension(&floated, width) + box_edges(&floated, true);
                let mut float_y = cursor;
                let (mut left, mut space) =
                    float_edges(&floats, content_x, content_width, float_y, 1.0);
                while outer_width > space && !floats.is_empty() {
                    let next = floats
                        .iter()
                        .map(|(r, _)| r.y + r.height)
                        .filter(|end| *end > float_y)
                        .min_by(f32::total_cmp);
                    let Some(next) = next else {
                        break;
                    };
                    float_y = next;
                    (left, space) = float_edges(&floats, content_x, content_width, float_y, 1.0);
                }
                let side = floated.float;
                let float_x = if side == Float::Right {
                    left + space - outer_width
                } else {
                    left
                };
                let height = self.box_for(
                    current,
                    &style,
                    Some(floated),
                    float_x,
                    float_y,
                    outer_width,
                    depth + 1,
                )?;
                if floats.len() == 4096 {
                    return Err(LayoutError::CommandLimit);
                }
                floats
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                floats.push((
                    Rect {
                        x: float_x,
                        y: float_y,
                        width: outer_width,
                        height,
                    },
                    side,
                ));
                if inline_width == 0.0 {
                    line_start = self.line_start();
                }
            } else if matches!(kind, NodeKind::Element { name, namespace: Namespace::Html, .. } if name == "br")
                && computed
                    .as_ref()
                    .is_some_and(|style| style.display != Display::None)
            {
                self.align_line(
                    line_start,
                    content_x,
                    content_width,
                    inline_width - trailing_space,
                    &style,
                    true,
                );
                cursor += inline_height.max(self.line_height(&style));
                inline_width = 0.0;
                inline_height = 0.0;
                line_start = self.line_start();
                trailing_space = 0.0;
                non_block_since = true;
                last_was_block = false;
                saw_non_block = true;
            } else if let NodeKind::Text(value) = kind {
                self.text_flow(
                    value,
                    &style,
                    content_x,
                    content_width,
                    &mut cursor,
                    &mut inline_width,
                    &mut inline_height,
                    &floats,
                    &mut line_start,
                    &mut trailing_space,
                )?;
                if !value.trim().is_empty() {
                    non_block_since = true;
                    last_was_block = false;
                    saw_non_block = true;
                }
            } else if computed.as_ref().is_some_and(|style| {
                matches!(style.display, Display::Inline | Display::InlineBlock)
            }) {
                if !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
                    && inline_width > 0.0
                    && inline_width >= content_width
                {
                    self.align_line(
                        line_start,
                        content_x,
                        content_width,
                        inline_width - trailing_space,
                        &style,
                        false,
                    );
                    cursor += inline_height;
                    inline_width = 0.0;
                    inline_height = 0.0;
                    line_start = self.line_start();
                    trailing_space = 0.0;
                }
                let first_command = self.commands.len();
                let first_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                let (width, height) = self.inline_for(
                    current,
                    &style,
                    computed,
                    content_x + inline_width,
                    cursor,
                    content_width,
                    depth + 1,
                )?;
                if !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
                    && inline_width > 0.0
                    && inline_width + width > content_width
                {
                    let wrapped = LineStart {
                        command: first_command,
                        hit: first_hit,
                    };
                    self.align_line_until(
                        line_start,
                        wrapped,
                        content_width,
                        inline_width - trailing_space,
                        &style,
                        false,
                    );
                    cursor += inline_height;
                    for command in &mut self.commands[first_command..] {
                        move_command(command, -inline_width, inline_height);
                    }
                    if let Some(geometry) = self.geometry.as_mut() {
                        geometry.move_hits(
                            first_hit..geometry.hits.len(),
                            -inline_width,
                            inline_height,
                        );
                    }
                    inline_width = 0.0;
                    inline_height = 0.0;
                    line_start = wrapped;
                    trailing_space = 0.0;
                }
                inline_width += width;
                inline_height = inline_height.max(height);
                non_block_since = true;
                last_was_block = false;
                saw_non_block = true;
            } else {
                self.align_line(
                    line_start,
                    content_x,
                    content_width,
                    inline_width - trailing_space,
                    &style,
                    true,
                );
                cursor += inline_height;
                inline_width = 0.0;
                inline_height = 0.0;
                trailing_space = 0.0;
                let multicol_column = (column_count > 1
                    && computed.as_ref().is_some_and(|child| {
                        child.display == Display::Block
                            && !matches!(child.position, Position::Absolute | Position::Fixed)
                    }))
                .then(|| (multicol_item_index / multicol_per_column).min(column_count - 1));
                if let Some(column) = multicol_column {
                    if previous_multicol_column != Some(column) {
                        previous_margin = 0.0;
                    }
                    cursor = multicol_cursors[column];
                }
                let (margin, margin_bottom, through_candidate) = match &computed {
                    Some(child_style) => {
                        if child_style.clear != Clear::None {
                            let before_clear = cursor;
                            for (rect, side) in &floats {
                                if child_style.clear == Clear::Both
                                    || child_style.clear == Clear::Left && *side == Float::Left
                                    || child_style.clear == Clear::Right && *side == Float::Right
                                {
                                    cursor = cursor.max(rect.y + rect.height);
                                }
                            }
                            // Actual clearance interrupts parent/child margin
                            // collapse; otherwise the final collapse pass can
                            // pull this child back up into the float (CSS 2.1 §9.5.2).
                            non_block_since |= cursor > before_clear;
                            previous_margin = 0.0;
                        }
                        let borderless = !child_style.border_solid
                            && child_style.padding_sides[0] == 0.0
                            && child_style.padding_sides[2] == 0.0;
                        let candidate = borderless
                            && child_style.display == Display::Block
                            && child_style.position == Position::Static
                            && child_style.float == Float::None
                            && child_style.clear == Clear::None
                            && !overflow_scroll_container(&child_style)
                            && !child_style.contain_paint
                            && !matches!(kind, NodeKind::Element { name, .. } if name == "img" || name == "canvas" || name == "video");
                        (
                            if vertical_block_flow {
                                0.0
                            } else {
                                child_style.margin_sides[0]
                            },
                            if vertical_block_flow {
                                0.0
                            } else {
                                child_style.margin_sides[2]
                            },
                            candidate && !vertical_block_flow,
                        )
                    }
                    None => (0.0, 0.0, false),
                };
                let collapsed =
                    previous_margin.max(margin).max(0.0) + previous_margin.min(margin).min(0.0);
                if previous_margin != 0.0 {
                    cursor += collapsed - previous_margin - margin;
                }
                let child_y = cursor;
                let (mut child_x, child_width) =
                    multicol_column.map_or((content_x, content_width), |column| {
                        (
                            content_x + column as f32 * (column_width + column_gap),
                            column_width,
                        )
                    });
                let vertical_outer_width = if vertical_block_flow {
                    computed.as_ref().map_or(content_width, |child| {
                        let border = if child.border_solid {
                            child.border_width
                        } else {
                            0.0
                        };
                        child.width.map_or(content_width, |width| {
                            content_dimension(child, width)
                                + child.padding_sides[1]
                                + child.padding_sides[3]
                                + 2.0 * border
                        }) + child.margin_sides[1]
                            + child.margin_sides[3]
                    })
                } else {
                    0.0
                };
                if vertical_block_flow {
                    child_x = if style.writing_mode == css::WritingMode::VerticalRl {
                        vertical_block_cursor - vertical_outer_width
                    } else {
                        vertical_block_cursor
                    };
                }
                let first_cmd = self.commands.len();
                let first_hit = self.geometry.as_ref().map_or(0, |g| g.hits.len());
                self.was_through = None;
                self.collapsed_bottom = None;
                let child_inline_extent = computed.as_ref().map_or(0.0, |child| {
                    child.height.map_or(0.0, |height| {
                        content_height_dimension(child, height)
                            + child.padding_sides[0]
                            + child.padding_sides[2]
                            + if child.border_solid {
                                2.0 * child.border_width
                            } else {
                                0.0
                            }
                    }) + child.margin_sides[0]
                        + child.margin_sides[2]
                });
                let advance = self.box_for(
                    current,
                    &style,
                    computed,
                    child_x,
                    cursor,
                    child_width,
                    depth + 1,
                )?;
                // A block child whose border box is empty collapses through:
                // its top margin, its own bottom margin and both adjoining
                // chain margins merge into one margin that the parent
                // materializes once (CSS 2.1 §8.3.1).
                let mut through = None;
                if through_candidate {
                    if let Some(chain) = self.was_through.take() {
                        if previous_margin != 0.0 {
                            cursor += previous_margin + margin - collapsed;
                        }
                        if chain > previous_margin {
                            cursor += chain - previous_margin;
                        }
                        previous_margin = previous_margin.max(chain);
                        through = Some(chain);
                    }
                }
                if through.is_none() {
                    previous_margin = self.collapsed_bottom.take().unwrap_or(margin_bottom);
                    if vertical_block_flow {
                        vertical_content_extent = vertical_content_extent.max(child_inline_extent);
                        vertical_block_cursor +=
                            if style.writing_mode == css::WritingMode::VerticalRl {
                                -vertical_outer_width
                            } else {
                                vertical_outer_width
                            };
                    } else if let Some(column) = multicol_column {
                        multicol_cursors[column] = cursor + advance;
                        cursor = multicol_cursors
                            .iter()
                            .copied()
                            .fold(column_start_y, f32::max);
                    } else {
                        cursor += advance;
                    }
                }
                if let Some(column) = multicol_column {
                    multicol_item_index += 1;
                    previous_multicol_column = Some(column);
                }
                if block_children.len() == 4096 {
                    return Err(LayoutError::CommandLimit);
                }
                block_children
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                block_children.push(BlockChild {
                    first_cmd,
                    first_hit,
                    border_top: child_y + margin,
                    margin_top: margin,
                    margin_bottom,
                    through,
                    advance,
                    break_before: non_block_since,
                });
                non_block_since = false;
                last_was_block = true;
                line_start = self.line_start();
            }
            if raised_flow.len() > raised_count {
                let (commands, hits) = raised_flow.last_mut().unwrap();
                commands.end = self.commands.len();
                hits.end = self.geometry.as_ref().map_or(0, |g| g.hits.len());
            }
            content_extent_width = content_extent_width.max(inline_width);
        }
        if generated_before_pending || generated_after_pending {
            let mut paragraph = InlineParagraph {
                has_preceding_content: inline_width > 0.0,
                ..InlineParagraph::default()
            };
            let mut frame_path = Vec::new();
            if generated_before_pending {
                let before = generated_before.as_ref().ok_or(LayoutError::InvalidTree)?;
                if before.style.display == Display::Inline {
                    self.append_virtual_generated_child(
                        before,
                        &style,
                        &mut paragraph,
                        &mut frame_path,
                    )?;
                } else {
                    if vertical_block_flow
                        || column_count > 1
                        || before.style.display != Display::Block
                    {
                        return Err(LayoutError::UnsupportedGeneratedContent);
                    }
                    self.flow_virtual_generated_block_child(
                        before,
                        &style,
                        content_x,
                        content_width,
                        &mut cursor,
                        &mut inline_width,
                        &mut inline_height,
                        &floats,
                        &mut line_start,
                        &mut trailing_space,
                        &mut previous_margin,
                        &mut block_children,
                        &mut non_block_since,
                        &mut last_was_block,
                        &mut content_extent_width,
                        depth + 1,
                    )?;
                    paragraph.has_preceding_content = false;
                }
            }
            if generated_after_pending {
                let after = generated_after.as_ref().ok_or(LayoutError::InvalidTree)?;
                if after.style.display == Display::Inline {
                    self.append_virtual_generated_child(
                        after,
                        &style,
                        &mut paragraph,
                        &mut frame_path,
                    )?;
                } else {
                    if vertical_block_flow
                        || column_count > 1
                        || after.style.display != Display::Block
                    {
                        return Err(LayoutError::UnsupportedGeneratedContent);
                    }
                    if !paragraph.frames.is_empty() || !paragraph.atoms.is_empty() {
                        self.flow_inline_paragraph(
                            &mut paragraph,
                            &style,
                            content_x,
                            content_width,
                            &mut cursor,
                            &mut inline_width,
                            &mut inline_height,
                            &floats,
                            &mut line_start,
                            &mut trailing_space,
                            depth + 1,
                        )?;
                        non_block_since = true;
                        last_was_block = false;
                        saw_non_block = true;
                        paragraph = InlineParagraph::default();
                        frame_path.clear();
                    }
                    self.flow_virtual_generated_block_child(
                        after,
                        &style,
                        content_x,
                        content_width,
                        &mut cursor,
                        &mut inline_width,
                        &mut inline_height,
                        &floats,
                        &mut line_start,
                        &mut trailing_space,
                        &mut previous_margin,
                        &mut block_children,
                        &mut non_block_since,
                        &mut last_was_block,
                        &mut content_extent_width,
                        depth + 1,
                    )?;
                }
            }
            if !paragraph.frames.is_empty() {
                self.flow_inline_paragraph(
                    &mut paragraph,
                    &style,
                    content_x,
                    content_width,
                    &mut cursor,
                    &mut inline_width,
                    &mut inline_height,
                    &floats,
                    &mut line_start,
                    &mut trailing_space,
                    depth + 1,
                )?;
                non_block_since = true;
                last_was_block = false;
                saw_non_block = true;
            }
        }
        self.align_line(
            line_start,
            content_x,
            content_width,
            inline_width - trailing_space,
            &style,
            true,
        );
        cursor += inline_height;
        if overflow_scroll_container(&style)
            || style.contain_paint
            || style.float != Float::None
            || matches!(style.position, Position::Absolute | Position::Fixed)
        {
            for (rect, _) in &floats {
                cursor = cursor.max(rect.y + rect.height);
            }
        }
        // CSS 2.1 §8.3.1: the container's own top/bottom margins collapse with
        // its first/last in-flow children when no border, padding or BFC root
        // separates them. Boxes that establish a new formatting context (BFC
        // roots) never collapse through with their children.
        let bfc_root = overflow_scroll_container(&style)
            || style.contain_paint
            || style.contain_layout
            || style.float != Float::None
            || !matches!(style.position, Position::Static)
            || style.transforms.is_some()
            || style.clear != Clear::None
            || !matches!(parent_style.display, Display::Block);
        let top_edge = !bfc_root && padding_top == 0.0 && border_width == 0.0;
        let bottom_edge = !bfc_root && padding_bottom == 0.0 && border_width == 0.0;
        let mut m_top = margin_top;
        let mut m_bottom = margin_bottom;
        let mut shift = 0.0f32;
        let mut top_chain_child = None;
        if top_edge && !block_children.is_empty() && !block_children[0].break_before {
            let mut chain_max = None::<f32>;
            for (index, child) in block_children.iter().enumerate() {
                if child.break_before {
                    break;
                }
                let value = match child.through {
                    Some(chain) => chain,
                    None => {
                        top_chain_child = Some(index);
                        child.margin_top
                    }
                };
                chain_max = Some(chain_max.map_or(value, |a| a.max(value)));
                if top_chain_child.is_some() {
                    break;
                }
            }
            if let Some(max) = chain_max {
                m_top = max.max(margin_top);
                if let Some(index) = top_chain_child {
                    // The merged chain moves above the container's border box;
                    // the first box-building child sits at the content top.
                    shift = outer_y + (m_top - margin_top) - block_children[index].border_top;
                }
            }
        }
        let all_through = !block_children.is_empty()
            && block_children
                .iter()
                .all(|child| child.through.is_some() && !child.break_before);
        let bottom_collapses = bottom_edge && last_was_block;
        if bottom_collapses {
            m_bottom = margin_bottom.max(previous_margin);
            self.collapsed_bottom = Some(m_bottom);
        }
        let chain_out_top =
            top_edge && (top_chain_child.is_some() || (all_through && !saw_non_block));
        let mut content_extent_height = if vertical_block_flow {
            vertical_content_extent
        } else if chain_out_top {
            // The top chain propagated above the border box: measure the
            // content from the first box-building child instead of the flow
            // cursor, which still carries the collapsed chain.
            match top_chain_child {
                Some(index) => cursor - block_children[index].border_top,
                None => 0.0,
            }
        } else {
            (cursor + scroll_y - outer_y - border_width - padding_top).max(0.0)
        }
        .max(0.0);
        if bottom_collapses {
            // The bottom chain propagates below the border box, so the
            // height excludes it.
            content_extent_height = (content_extent_height - previous_margin).max(0.0);
        }
        let has_intrinsic_height = style.height_intrinsic.is_some()
            || style.min_height_intrinsic.is_some()
            || style.max_height_intrinsic.is_some();
        let intrinsic_content_height = if has_intrinsic_height {
            // Intrinsic block-size keywords measure the contents independently of
            // the preferred block size. In particular, min-height:max-content
            // must be able to raise a definite height, and max-height:min-content
            // must be able to clamp one. Passing the specified height through to
            // intrinsic_size would make both keywords measure that definite value
            // instead of the contents.
            let mut intrinsic_style = style.clone();
            intrinsic_style.height = None;
            intrinsic_style.height_intrinsic = None;
            (self.intrinsic_size(id, &intrinsic_style, depth + 1)?.1
                - box_edges(&intrinsic_style, false))
            .max(0.0)
        } else {
            0.0
        };
        let preferred_height = style.height_intrinsic.map_or_else(
            || {
                style.height.map_or(
                    if style.contain_size {
                        0.0
                    } else {
                        content_extent_height
                    },
                    |height| content_height_dimension(&style, height),
                )
            },
            |_| intrinsic_content_height,
        );
        let minimum_height = style
            .min_height_intrinsic
            .map_or(style.min_height, |_| intrinsic_content_height);
        let maximum_height = style
            .max_height_intrinsic
            .map_or(style.max_height, |_| Some(intrinsic_content_height));
        let content_height = preferred_height
            .min(maximum_height.unwrap_or(f32::INFINITY))
            .max(minimum_height)
            .max(0.0);
        let box_height = content_height + padding_top + padding_bottom + border_top + border_bottom;
        // Empty containers with collapsible top and bottom edges collapse
        // through completely: the parent materializes the single merged margin.
        let collapse_through = box_height == 0.0
            && top_edge
            && bottom_edge
            && !saw_non_block
            && block_children.iter().all(|child| child.through.is_some());
        if collapse_through {
            self.was_through = Some(m_top.max(m_bottom).max(previous_margin));
        }
        let paint_y = outer_y + if top_edge { m_top - margin_top } else { 0.0 };
        if shift != 0.0 {
            if let Some(first) = top_chain_child.map(|index| &block_children[index]) {
                for command in &mut self.commands[first.first_cmd..] {
                    move_command(command, 0.0, shift);
                }
                if let Some(geometry) = self.geometry.as_mut() {
                    let last_hit = geometry.hits.len();
                    geometry.move_hits(first.first_hit..last_hit, 0.0, shift);
                }
            }
        }
        if style.position != Position::Static || style.transforms.is_some() {
            self.containing_block = Some(Rect {
                x: outer_x + border_width,
                y: outer_y + border_width,
                width: box_width - 2.0 * border_width,
                height: box_height - 2.0 * border_width,
            });
            if style.transforms.is_some() {
                self.fixed_containing_block = self.containing_block;
            }
        }
        // Appendix E layer 8: in-flow positioned/transformed/opacity contexts
        // paint above ordinary flow, with hit order following the same chunks.
        self.raise_flow_paint(&mut raised_flow);
        let positioned_order_max = if positioned
            .iter()
            .any(|(_, child, ..)| child.z_index.is_some_and(|z| z > 0))
        {
            None
        } else {
            positioned.iter().map(|entry| entry.4).max()
        };
        if !negatives_laid.is_empty()
            || positioned
                .iter()
                .any(|(_, s, ..)| s.z_index.is_some_and(|z| z > 0))
        {
            let (mut ordered, mut positive) = (Vec::new(), Vec::new());
            for entry in positioned.into_iter() {
                let (node, child_style, x, y, index) = entry;
                if negatives_laid.contains(&node) {
                    continue;
                }
                if child_style.z_index.is_some_and(|z| z > 0) {
                    positive.push((node, child_style, x, y, index));
                } else {
                    ordered.push((node, child_style, x, y, index));
                }
            }
            ordered.extend(positive.drain(..));
            ordered.sort_by_key(|(_, child, ..)| child.z_index.unwrap_or(0));
            for (node, child_style, x, y, _) in ordered {
                self.box_for(
                    node,
                    &style,
                    Some(child_style),
                    x,
                    y,
                    content_width,
                    depth + 1,
                )?;
            }
        } else {
            for (node, child_style, x, y, _) in positioned {
                self.box_for(
                    node,
                    &style,
                    Some(child_style),
                    x,
                    y,
                    content_width,
                    depth + 1,
                )?;
            }
        }
        if let Some(last_positioned_order) = positioned_order_max {
            let mut later_relative = Vec::new();
            let mut index = positioned_flow_start;
            while index < self.positioned_flow_paints.len() {
                if self.positioned_flow_paints[index].order > last_positioned_order {
                    later_relative.push(self.positioned_flow_paints.remove(index));
                } else {
                    index += 1;
                }
            }
            if !later_relative.is_empty() {
                later_relative.sort_by_key(|paint| paint.order);
                let mut chunks: Vec<_> = later_relative
                    .iter()
                    .map(|paint| (paint.commands.clone(), paint.hits.clone()))
                    .collect();
                self.raise_flow_paint(&mut chunks);
            }
        }
        if column_count > 1 && style.column_rule_visible && style.column_rule_width > 0.0 {
            let color = style.column_rule_color.unwrap_or(style.color);
            let top = outer_y + border_width + padding_top;
            for column in 1..column_count {
                let center =
                    content_x + column as f32 * column_width + (column as f32 - 0.5) * column_gap;
                self.push_command(Command::FillRect {
                    rect: Rect {
                        x: center - style.column_rule_width * 0.5,
                        y: top,
                        width: style.column_rule_width,
                        height: content_height,
                    },
                    color,
                })?;
            }
        }
        self.cull = saved_cull;
        if column_count > 1 {
            self.scroll_hazards += 1;
        }
        if let Some(index) = clip_index {
            let padding = Rect {
                x: outer_x + border_left,
                y: outer_y + border_top,
                width: content_width + padding_left + padding_right,
                height: content_height + padding_top + padding_bottom,
            };
            let clip = overflow_clip_rect(padding, clip_x, clip_y);
            let clip_radius = if clip_x && clip_y {
                (style.border_radius - border_width).max(0.0)
            } else {
                0.0
            };
            let content_end = self.commands.len();
            let replayable =
                scroll_window.filter(|(_, _, hazards)| *hazards == self.scroll_hazards);
            let border_box = Rect {
                x: outer_x,
                y: outer_y,
                width: content_width + padding_left + padding_right + border_left + border_right,
                height: content_height + padding_top + padding_bottom + border_top + border_bottom,
            };
            let clip_corners = if clip_x && clip_y {
                clipped_corners(border_box, padding, &style)
            } else {
                None
            };
            if clip_radius > 0.0 || clip_corners.is_some() {
                self.replace_command(
                    index,
                    Command::PushLayer {
                        corners: clip_corners.clone(),
                        rect: clip,
                        radius: clip_radius,
                        opacity: 1.0,
                        clip: true,
                    },
                )?;
                self.push_command(Command::PopLayer)?;
            } else {
                self.replace_command(index, Command::PushClip(clip))?;
                self.push_command(Command::PopClip)?;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if geometry.rounded_clips.len() == MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                geometry
                    .rounded_clips
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                if let Some((base, window, _)) = replayable {
                    geometry
                        .scroll_regions
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    geometry.scroll_regions.push(ScrollRegion {
                        node: id,
                        commands: index + 1..content_end,
                        hits: first_child_hit..geometry.hits.len(),
                        transforms: scroll_transforms..geometry.transforms.len(),
                        clips: scroll_clips..geometry.rounded_clips.len(),
                        base,
                        window,
                    });
                }
                geometry.rounded_clips.push(HitClip {
                    corners: clip_corners,
                    first_transform: geometry.transforms.len(),
                    hits: first_child_hit..geometry.hits.len(),
                    rect: clip,
                    radius: clip_radius,
                });
                if scroll_container {
                    geometry
                        .scroll_extents
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    geometry.scroll_extents.push(ScrollOffset {
                        node: id,
                        x: if scroll_x_container {
                            (content_extent_width - content_width).max(0.0)
                        } else {
                            0.0
                        },
                        y: if scroll_y_container {
                            (content_extent_height - content_height).max(0.0)
                        } else {
                            0.0
                        },
                    });
                }
            }
        }
        self.finish_hit(
            hit,
            Rect {
                x: outer_x,
                y: paint_y,
                width: box_width,
                height: box_height,
            },
        );
        if let Some(index) = background_index {
            self.finish_background(
                index,
                Rect {
                    x: outer_x,
                    y: paint_y,
                    width: box_width,
                    height: box_height,
                },
                &style,
                Some(Rect {
                    x: outer_x + border_left - scroll_x,
                    y: outer_y + border_top - scroll_y,
                    width: content_extent_width.max(content_width) + padding_left + padding_right,
                    height: content_extent_height.max(content_height)
                        + padding_top
                        + padding_bottom,
                }),
            )?;
        }
        if let Some(index) = border_index {
            self.replace_command(
                index,
                border(
                    Rect {
                        x: outer_x,
                        y: paint_y,
                        width: box_width,
                        height: box_height,
                    },
                    &style,
                ),
            )?;
        } else if self.suppressed_border_node != Some(id)
            && style.visibility_visible
            && has_nonuniform_border(&style)
        {
            for command in side_border_commands(
                Rect {
                    x: outer_x,
                    y: paint_y,
                    width: box_width,
                    height: box_height,
                },
                &style,
            ) {
                self.push_command(command)?;
            }
        }
        if let Some((target_cmd, target_hit)) = sc_root {
            self.rotate_stacking_escapes(target_cmd, target_hit);
            self.stacking_roots.pop();
        }
        Ok(if collapse_through {
            0.0
        } else {
            box_height + m_top + m_bottom
        })
    }
}

pub fn display_list(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
) -> Result<DisplayList, LayoutError> {
    let mut rules = stylesheets(document)?;
    rules.environment = css::MediaEnvironment {
        width: width as f32,
        height: height as f32,
        ..Default::default()
    };
    display_list_with_styles(document, width, height, text, &rules, None, None, &[])
}

pub fn display_list_with_images(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    images: &dyn ImageResolver,
) -> Result<DisplayList, LayoutError> {
    let mut rules = stylesheets(document)?;
    rules.environment = css::MediaEnvironment {
        width: width as f32,
        height: height as f32,
        ..Default::default()
    };
    display_list_with_styles(
        document,
        width,
        height,
        text,
        &rules,
        Some(images),
        None,
        &[],
    )
}

pub(crate) fn display_list_with_styles(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    rules: &StyleIndex,
    images: Option<&dyn ImageResolver>,
    geometry: Option<&mut LayoutGeometry>,
    scrolls: &[ScrollOffset],
) -> Result<DisplayList, LayoutError> {
    let style_cache = core::cell::RefCell::default();
    display_list_with_retained_styles(
        document,
        width,
        height,
        text,
        rules,
        images,
        geometry,
        scrolls,
        &style_cache,
    )
}

pub(crate) fn display_list_with_retained_styles(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    rules: &StyleIndex,
    images: Option<&dyn ImageResolver>,
    geometry: Option<&mut LayoutGeometry>,
    scrolls: &[ScrollOffset],
    style_cache: &core::cell::RefCell<css::StyleCache>,
) -> Result<DisplayList, LayoutError> {
    display_list_with_retained_layout(
        document,
        width,
        height,
        text,
        rules,
        images,
        geometry,
        scrolls,
        style_cache,
        None,
    )
}

pub(crate) fn display_list_with_retained_layout(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    rules: &StyleIndex,
    images: Option<&dyn ImageResolver>,
    geometry: Option<&mut LayoutGeometry>,
    scrolls: &[ScrollOffset],
    style_cache: &core::cell::RefCell<css::StyleCache>,
    retained_fragments: Option<&mut RetainedLayoutCache>,
) -> Result<DisplayList, LayoutError> {
    display_list_with_retained_layout_and_root(
        document,
        width,
        height,
        text,
        rules,
        images,
        geometry,
        scrolls,
        style_cache,
        retained_fragments,
        None,
    )
}

pub(crate) fn display_list_with_retained_layout_and_root(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    rules: &StyleIndex,
    images: Option<&dyn ImageResolver>,
    geometry: Option<&mut LayoutGeometry>,
    scrolls: &[ScrollOffset],
    style_cache: &core::cell::RefCell<css::StyleCache>,
    retained_fragments: Option<&mut RetainedLayoutCache>,
    presentation_root: Option<NodeId>,
) -> Result<DisplayList, LayoutError> {
    let root = document.root();
    let mut html = None;
    let mut svg_document = None;
    let mut child = document
        .first_child(root)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        if let Ok(NodeKind::Element {
            name, namespace, ..
        }) = document.kind(current)
        {
            if *namespace == Namespace::Html && crate::svg::local_name(name) == "html" {
                html = Some(current);
                break;
            }
            if *namespace == Namespace::Svg && crate::svg::local_name(name) == "svg" {
                svg_document = Some(current);
            }
        }
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    let html = html.or(svg_document).ok_or(LayoutError::InvalidTree)?;
    let standalone_svg = matches!(
        document.kind(html),
        Ok(NodeKind::Element {
            namespace: Namespace::Svg,
            name,
            ..
        }) if crate::svg::local_name(name) == "svg"
    );
    let mut layout = Layout {
        document,
        text,
        rules,
        images,
        commands: Vec::new(),
        command_bytes: 0,
        body_background_on_canvas: false,
        viewport: Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        geometry,
        scrolls,
        cull: Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        scroll_hazards: 0,
        containing_block: None,
        fixed_containing_block: None,
        sticky_bounds: None,
        transform_depth: 0,
        parent_height: None,
        decorations: Vec::new(),
        collapsed_bottom: None,
        was_through: None,
        stacking_roots: Vec::new(),
        pending_escapes: Vec::new(),
        positioned_flow_paints: Vec::new(),
        next_paint_order: 0,
        intrinsic_cache: core::cell::RefCell::new(Vec::new()),
        style_cache,
        retained_fragments,
        suppressed_border_node: None,
    };
    layout.push_command(Command::FillRect {
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        color: Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        },
    })?;
    let initial = layout
        .computed_style(html, None)
        .map_err(LayoutError::Css)?;
    if initial.display == Display::None {
        return layout.finish_display_list();
    }
    let root_hit = layout.begin_hit(html)?;
    layout.finish_hit(root_hit, layout.viewport);
    if has_background(&initial) {
        layout.push_background(layout.viewport, &initial)?;
    }
    if standalone_svg {
        let NodeKind::Element { attributes, .. } =
            document.kind(html).map_err(|_| LayoutError::InvalidTree)?
        else {
            return Err(LayoutError::InvalidTree);
        };
        let mut style = initial;
        if crate::svg::attribute(attributes, "width").is_none() && style.width.is_none() {
            style.width = Some(width as f32);
        }
        if crate::svg::attribute(attributes, "height").is_none() && style.height.is_none() {
            style.height = Some(height as f32);
        }
        let parent_height = layout.parent_height;
        layout.parent_height = Some(height as f32);
        layout.paint_svg_root(html, attributes, &style, 0.0, 0.0, width as f32, 0)?;
        layout.parent_height = parent_height;
        return layout.finish_display_list();
    }
    if let Some(root) = presentation_root {
        if !matches!(document.kind(root), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        let mut top_layer = layout
            .computed_style(root, Some(&initial))
            .map_err(LayoutError::Css)?;
        if top_layer.display == Display::None {
            return layout.finish_display_list();
        }
        // Fullscreen is a viewport presentation layer. Preserve the selected
        // element's computed paint/text styles while applying the CSS
        // Fullscreen UA inset rule for its top-layer box.
        top_layer.position = Position::Fixed;
        top_layer.top = Some(0.0);
        top_layer.right = Some(0.0);
        top_layer.bottom = Some(0.0);
        top_layer.left = Some(0.0);
        top_layer.margin = 0.0;
        top_layer.margin_sides = [0.0; 4];
        let root_target = (
            layout.commands.len(),
            layout
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len()),
        );
        layout.stacking_roots.push(root_target);
        layout.box_for(root, &initial, Some(top_layer), 0.0, 0.0, width as f32, 0)?;
        layout.stacking_roots.pop();
        layout.rotate_stacking_escapes(root_target.0, root_target.1);
        return layout.finish_display_list();
    }
    let mut child = document
        .first_child(html)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        if matches!(document.kind(current), Ok(NodeKind::Element { name, namespace: Namespace::Html, .. }) if crate::svg::local_name(name) == "body")
        {
            let style = layout
                .computed_style(current, Some(&initial))
                .map_err(LayoutError::Css)?;
            if style.display != Display::None && has_background(&style) && !has_background(&initial)
            {
                layout.push_background(
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: width as f32,
                        height: height as f32,
                    },
                    &style,
                )?;
                layout.body_background_on_canvas = true;
            }
            let root_target = (
                layout.commands.len(),
                layout
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len()),
            );
            layout.stacking_roots.push(root_target);
            layout.box_for(current, &initial, None, 0.0, 0.0, width as f32, 0)?;
            layout.stacking_roots.pop();
            layout.rotate_stacking_escapes(root_target.0, root_target.1);
        }
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    layout.finish_display_list()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::ShapedRun;

    struct NoShape;

    impl TextShaper for NoShape {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            panic!("offscreen text was shaped")
        }
        fn ascent(&self, _: f32) -> f32 {
            10.0
        }
        fn line_height(&self, _: f32) -> f32 {
            20.0
        }
    }

    #[test]
    fn svg_capability_query_detects_applied_unsupported_css_and_elements() {
        let document = crate::html::parse(
            "<style>svg rect { filter: blur(1px) }</style><svg><rect width='2' height='2'/><use href='https://example.test/shape.svg#shape'/></svg>",
            128,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let unsupported = unsupported_svg_features(&document, &rules).unwrap();
        assert!(unsupported.contains(&SvgUnsupportedFeature::Attribute("filter".into())));
        assert!(unsupported.contains(&SvgUnsupportedFeature::Element("use-reference".into())));
    }

    #[test]
    fn svg_capability_query_accepts_a_resolved_linear_gradient_fallback() {
        let document = crate::html::parse(
            "<svg><defs><linearGradient id='g'><stop offset='0' stop-color='red'/></linearGradient></defs><rect width='2' height='2' fill='url(#g) blue'/></svg>",
            128,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let unsupported = unsupported_svg_features(&document, &rules).unwrap();
        assert!(!unsupported.contains(&SvgUnsupportedFeature::PaintServer));
    }

    #[test]
    fn svg_capability_query_accepts_local_shape_use_and_clip_path() {
        let document = crate::html::parse(
            "<svg><defs><g id='shape'><rect width='3' height='3'/></g><clipPath id='clip'><rect width='2' height='3'/></clipPath></defs><use href='#shape' x='1' clip-path='url(#clip)'/></svg>",
            128,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let unsupported = unsupported_svg_features(&document, &rules).unwrap();
        assert!(!unsupported.contains(&SvgUnsupportedFeature::Element("use-reference".into())));
        assert!(!unsupported.contains(&SvgUnsupportedFeature::PaintServer));
    }

    #[test]
    fn svg_capability_query_rejects_cyclic_use_and_container_object_box_clip_paths() {
        let document = crate::html::parse(
            "<svg><defs><g id='loop'><use href='#loop'/></g><clipPath id='clip' clipPathUnits='objectBoundingBox'><rect width='1' height='1'/></clipPath></defs><use href='#loop'/><g clip-path='url(#clip)'><rect width='4' height='4'/></g></svg>",
            128,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let unsupported = unsupported_svg_features(&document, &rules).unwrap();
        assert!(unsupported.contains(&SvgUnsupportedFeature::Element("use-reference".into())));
        assert!(unsupported.contains(&SvgUnsupportedFeature::PaintServer));
    }

    #[test]
    fn svg_capability_query_accepts_object_box_clip_on_a_shape() {
        let document = crate::html::parse(
            "<svg><defs><clipPath id='clip' clipPathUnits='objectBoundingBox'><rect width='.5' height='1'/></clipPath></defs><rect width='4' height='4' clip-path='url(#clip)'/></svg>",
            128,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let unsupported = unsupported_svg_features(&document, &rules).unwrap();
        assert!(!unsupported.contains(&SvgUnsupportedFeature::PaintServer));
    }

    #[test]
    fn offscreen_text_skips_shaping_and_paint() {
        let document =
            crate::html::parse("<div style='height:100px'></div><p>below viewport</p>", 64)
                .unwrap();
        let list = display_list(&document, 20, 10, &NoShape).unwrap();
        assert!(
            !list
                .0
                .iter()
                .any(|command| matches!(command, Command::GlyphRun { .. }))
        );
    }

    #[test]
    fn offscreen_sized_image_skips_loading() {
        let document = crate::html::parse(
            "<div style='height:100px'></div><img src='large.png' width='20' height='10'>",
            64,
        )
        .unwrap();
        let images = |_: &str| -> ImageState { panic!("offscreen image was loaded") };
        display_list_with_images(&document, 20, 10, &NoShape, &images).unwrap();
    }

    #[test]
    fn display_list_rejects_referenced_image_payload_over_the_byte_budget() {
        let document = crate::html::parse("<img src='large.png'>", 32).unwrap();
        let image = Arc::new(ImageData {
            width: 2048,
            height: 2048,
            pixels: alloc::vec![0; 16 * 1024 * 1024],
        });
        let images = |_: &str| ImageState::Ready(image.clone());
        assert_eq!(
            display_list_with_images(&document, 100, 100, &FixedText, &images),
            Err(LayoutError::CommandLimit)
        );
    }

    fn background_fills(
        markup: &str,
        images: impl Fn(&str) -> ImageState,
    ) -> Vec<(Rect, Rect, Rect, [BackgroundRepeat; 2])> {
        let markup = alloc::format!("<style>body{{margin:0}}</style>{markup}");
        let document = crate::html::parse(&markup, 32).unwrap();
        display_list_with_images(&document, 100, 100, &NoShape, &images)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillBackground(fill) => Some((
                    fill.rect,
                    fill.positioning_rect,
                    fill.image_rect,
                    fill.repeat,
                )),
                _ => None,
            })
            .collect()
    }

    fn test_image(source: &str) -> ImageState {
        match source {
            "a.png" => ImageState::Ready(Arc::new(ImageData {
                width: 8,
                height: 4,
                pixels: alloc::vec![200; 8 * 4 * 4],
            })),
            _ => ImageState::Failed,
        }
    }

    #[test]
    fn background_url_layers_position_size_and_clip_follow_the_boxes() {
        let (rect, positioning, image_rect, repeat) = background_fills(
            "<style>body{margin:0}</style><main style='width:40px;height:30px;padding:4px;border:2px solid black;background-image:url(a.png);background-position:10px 5px;background-size:20px 10px;background-repeat:no-repeat;background-clip:content-box;background-origin:content-box'></main>",
            test_image,
        )[0];
        // Border box 44x38, content box inset by border and padding.
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (6.0, 6.0, 40.0, 30.0)
        );
        assert_eq!(
            (
                positioning.x,
                positioning.y,
                positioning.width,
                positioning.height
            ),
            (6.0, 6.0, 40.0, 30.0)
        );
        assert_eq!(
            (
                image_rect.x,
                image_rect.y,
                image_rect.width,
                image_rect.height
            ),
            (16.0, 11.0, 20.0, 10.0)
        );
        assert_eq!(repeat, [BackgroundRepeat::NoRepeat; 2]);
    }

    #[test]
    fn background_color_uses_clip_of_bottom_most_image_layer() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='width:120px;height:100px;background-color:green;background-clip:border-box,content-box,border-box;background-image:none,none;border:10px solid transparent'></div>",
            16,
        )
        .unwrap();
        let list = display_list(&document, 200, 160, &FixedText).unwrap();
        let rect = list
            .0
            .iter()
            .find_map(|command| match command {
                Command::FillRect { rect, color } if color.g > color.r => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            rect,
            Rect {
                x: 10.0,
                y: 10.0,
                width: 120.0,
                height: 100.0,
            }
        );
    }

    #[test]
    fn background_cover_contain_and_percent_position_resolve_against_the_area() {
        // Image 8x4 (ratio 2) in a 40x20 area: cover and contain both fill it.
        let (.., image_rect, repeat) = background_fills(
            "<style>body{margin:0}</style><div style='width:40px;height:20px;background-image:url(a.png);background-position:25% 50%;background-size:cover'></div>",
            test_image,
        )[0];
        assert_eq!(
            (
                image_rect.x,
                image_rect.y,
                image_rect.width,
                image_rect.height
            ),
            (0.0, 0.0, 40.0, 20.0)
        );
        assert_eq!(repeat, [BackgroundRepeat::Repeat; 2]);
        // Contain in a wider 40x10 area keeps the ratio inside the height; the
        // 25%/50% position resolves against the free space around the tile.
        let (.., image_rect, _) = background_fills(
            "<style>body{margin:0}</style><div style='width:40px;height:10px;background-image:url(a.png);background-position:25% 50%;background-size:contain;background-repeat:no-repeat'></div>",
            test_image,
        )[0];
        assert_eq!(
            (
                image_rect.x,
                image_rect.y,
                image_rect.width,
                image_rect.height
            ),
            (5.0, 0.0, 20.0, 10.0)
        );
        // Explicit percentage size resolves against the positioning area.
        let (.., image_rect, _) = background_fills(
            "<style>body{margin:0}</style><div style='width:40px;height:20px;background-image:url(a.png);background-size:50% 100%'></div>",
            test_image,
        )[0];
        assert_eq!(
            (
                image_rect.x,
                image_rect.y,
                image_rect.width,
                image_rect.height
            ),
            (0.0, 0.0, 20.0, 20.0)
        );
    }

    #[test]
    fn fixed_background_uses_viewport_area_and_keeps_element_clip() {
        let (clip, positioning, image, _) = background_fills(
            "<div style='width:30px;height:20px;background-image:url(a.png);background-attachment:fixed;background-position:right bottom;background-repeat:no-repeat'></div>",
            test_image,
        )[0];
        assert_eq!(
            positioning,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            }
        );
        assert_eq!(clip.width, 30.0);
        assert_eq!(clip.height, 20.0);
        assert_eq!(image.x, 92.0);
        assert_eq!(image.y, 96.0);
    }

    #[test]
    fn local_background_uses_scrollable_padding_area() {
        let (clip, positioning, _, _) = background_fills(
            "<div style='width:30px;height:20px;overflow:auto;background-image:url(a.png);background-attachment:local'><div style='height:80px;margin:0'></div></div>",
            test_image,
        )[0];
        assert_eq!(clip.height, 20.0);
        assert_eq!(positioning.height, 80.0);
    }

    #[test]
    fn one_axis_overflow_clip_does_not_scroll_or_clip_the_other_axis() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='width:20px;height:20px;overflow-x:clip'><div style='width:100px;height:100px;margin:0'></div></div>",
            64,
        )
        .unwrap();
        let mut rules = stylesheets(&document).unwrap();
        rules.environment = css::MediaEnvironment {
            width: 100.0,
            height: 100.0,
            ..Default::default()
        };
        let mut geometry = LayoutGeometry::default();
        let list = display_list_with_styles(
            &document,
            100,
            100,
            &NoShape,
            &rules,
            None,
            Some(&mut geometry),
            &[],
        )
        .unwrap();
        assert!(list.0.iter().any(|command| matches!(
            command,
            Command::PushClip(Rect { x: 0.0, width: 20.0, height, .. }) if *height > 1.0e19
        )));
        assert!(geometry.scroll_extents.is_empty());
        assert!(geometry.scroll_regions.is_empty());
    }

    #[test]
    fn stretched_background_masks_resolve_final_border_geometry() {
        for display in ["flex", "grid"] {
            for background in ["red", "linear-gradient(red,blue)"] {
                let fixture = alloc::format!(
                    "<style>body{{margin:0}}</style><div style='display:{display};height:80px;width:100px'><div style='width:40px;border:4px double transparent;border-radius:50%;box-shadow:inset 1px 2px red;background:{background};background-clip:border-area text'>text</div></div>"
                );
                let document = crate::html::parse(&fixture, 64).unwrap();
                let list = crate::layout::display_list(&document, 100, 100, &FixedText).unwrap();
                let mask = list
                    .0
                    .iter()
                    .find_map(|command| match command {
                        Command::MaskedBackground(mask) => Some(mask),
                        _ => None,
                    })
                    .expect("masked stretched background");
                assert_eq!(mask.rect.height, 80.0, "{display}: {background}");
                let border = mask
                    .mask
                    .0
                    .iter()
                    .find_map(|command| match command {
                        Command::StrokeBoxBorder(border) => Some(border),
                        _ => None,
                    })
                    .expect("border-area coverage");
                assert_eq!(border.rect, mask.rect);
                assert_eq!(border.corners.unwrap()[0][1], 40.0);
                let shadow = list
                    .0
                    .iter()
                    .find_map(|command| match command {
                        Command::BoxShadow {
                            rect,
                            corners,
                            shadow,
                            ..
                        } if shadow.inset => Some((rect, corners)),
                        _ => None,
                    })
                    .expect("stretched inset shadow");
                assert_eq!(shadow.0.height, 72.0);
                assert_eq!(shadow.1.as_ref().unwrap()[0][1], 36.0);
                assert!(
                    mask.mask
                        .0
                        .iter()
                        .any(|command| matches!(command, Command::GlyphRun { .. })),
                    "stretch retains the ink mask"
                );
            }
        }
    }

    #[test]
    fn ancestor_text_background_requires_scroll_mask_repaint() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='background:red;background-clip:text'><div style='width:30px;height:20px;overflow:auto'><div style='height:80px'>text</div></div></div>",
            64,
        ).unwrap();
        let mut rules = stylesheets(&document).unwrap();
        rules.environment = css::MediaEnvironment {
            width: 100.0,
            height: 100.0,
            ..Default::default()
        };
        let mut geometry = LayoutGeometry::default();
        let mut list = display_list_with_styles(
            &document,
            100,
            100,
            &FixedText,
            &rules,
            None,
            Some(&mut geometry),
            &[],
        )
        .unwrap();
        let region = geometry
            .scroll_regions
            .first()
            .expect("ordinary descendant scroll region")
            .clone();
        assert!(
            list.0
                .iter()
                .enumerate()
                .any(|(index, command)| !region.commands.contains(&index)
                    && matches!(command,Command::MaskedBackground(mask) if mask.text_clipped))
        );
        let before = list.0.clone();
        assert!(!geometry.scroll_region(&mut list.0, region.node, (0.0, 0.0), (0.0, 1.0)));
        assert_eq!(
            list.0, before,
            "fallback must leave paint unchanged until relayout"
        );
    }

    #[test]
    fn fixed_and_local_backgrounds_disable_unsafe_scroll_replay() {
        let fixtures = [
            "<style>body{margin:0}</style><div style='width:30px;height:20px;overflow:auto;background-image:url(a.png)'><div style='height:80px;margin:0;background-image:url(a.png);background-attachment:fixed'></div></div>",
            "<style>body{margin:0}</style><div style='width:30px;height:20px;overflow:auto;background-image:url(a.png);background-attachment:local'><div style='height:80px;margin:0'></div></div>",
        ];
        for (fixture, attachment) in fixtures.into_iter().zip(["fixed", "local"]) {
            let document = crate::html::parse(fixture, 64).unwrap();
            let mut rules = stylesheets(&document).unwrap();
            rules.environment = css::MediaEnvironment {
                width: 100.0,
                height: 100.0,
                ..Default::default()
            };
            let mut geometry = LayoutGeometry::default();
            display_list_with_styles(
                &document,
                100,
                100,
                &NoShape,
                &rules,
                None,
                Some(&mut geometry),
                &[],
            )
            .unwrap();
            assert!(
                geometry.scroll_regions.is_empty(),
                "{attachment} attachment must not use unsafe translate-only replay"
            );
        }
    }

    #[test]
    fn round_background_repeat_adjusts_auto_size_and_position() {
        let (.., image_rect, repeat) = background_fills(
            "<div style='width:30px;height:20px;background-image:url(a.png);background-repeat:round no-repeat;background-position:right bottom'></div>",
            test_image,
        )[0];
        assert_eq!(
            repeat,
            [BackgroundRepeat::Round, BackgroundRepeat::NoRepeat]
        );
        assert!(
            (image_rect.x - 22.5).abs() < 0.001
                && (image_rect.y - 16.25).abs() < 0.001
                && (image_rect.width - 7.5).abs() < 0.001
                && (image_rect.height - 3.75).abs() < 0.001,
            "one-axis round should keep the intrinsic ratio and reposition the tile: {image_rect:?}"
        );

        let (.., image_rect, repeat) = background_fills(
            "<div style='width:30px;height:20px;background-image:url(a.png);background-repeat:round;background-position:0 0'></div>",
            test_image,
        )[0];
        assert_eq!(repeat, [BackgroundRepeat::Round; 2]);
        assert!(
            (image_rect.width - 7.5).abs() < 0.001 && (image_rect.height - 4.0).abs() < 0.001,
            "two-axis round should adjust each axis independently: {image_rect:?}"
        );
    }

    #[test]
    fn background_url_pending_errors_and_failed_paints_nothing() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='width:40px;height:20px;background-image:url(a.png)'></div>",
            32,
        )
        .unwrap();
        let pending = |_: &str| ImageState::Pending;
        assert_eq!(
            display_list_with_images(&document, 100, 100, &NoShape, &pending),
            Err(LayoutError::ImagePending)
        );
        let fills = background_fills(
            "<style>body{margin:0}</style><div style='width:40px;height:20px;background-image:url(missing.png)'></div>",
            test_image,
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].2.width, 0.0, "failed images paint no tile");
    }

    #[test]
    fn background_gradients_flow_through_layers_in_paint_order() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='width:40px;height:20px;background-image:linear-gradient(red,blue),url(a.png)'></div>",
            32,
        )
        .unwrap();
        let list = display_list_with_images(&document, 100, 100, &NoShape, &test_image).unwrap();
        let paints = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillBackground(fill) => Some((&fill.rect, &fill.image)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(paints.len(), 2);
        // The first layer paints last, so the url tile is under the gradient.
        assert!(matches!(&paints[0].1, BackgroundPaint::Image(_)));
        assert!(matches!(&paints[1].1, BackgroundPaint::Gradient(_)));
        // A gradient has no intrinsic size: auto fills the positioning area.
        assert_eq!((paints[1].0.width, paints[1].0.height), (40.0, 20.0));
    }

    struct FixedText;

    struct SizedText;

    impl TextShaper for SizedText {
        fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun {
                glyphs: Arc::from([]),
                width: text.chars().count() as f32 * size * 0.5,
            })
        }

        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }

        fn line_height(&self, size: f32) -> f32 {
            size
        }
    }

    #[test]
    fn table_cells_align_default_baselines_and_top_middle_bottom_contents() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}td{padding:0}</style><table><tr><td style='font-size:10px'>a</td><td style='font-size:20px'>b</td></tr></table>",
            32,
        )
        .unwrap();
        let list = display_list(&document, 100, 80, &SizedText).unwrap();
        let baselines = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(baselines.len(), 2);
        assert_eq!(baselines, [16.0, 16.0]);

        let document = crate::html::parse(
            "<style>html,body{margin:0}td{padding:0;font-size:10px}</style><table style='height:30px;border-spacing:0'><tr><td style='vertical-align:top'>a</td><td style='vertical-align:middle'>b</td><td style='vertical-align:bottom'>c</td></tr></table>",
            32,
        )
        .unwrap();
        let list = display_list(&document, 100, 80, &SizedText).unwrap();
        let baselines = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(baselines, [8.0, 18.0, 28.0]);
    }

    #[test]
    fn table_fixed_cells_share_columns_and_spans() {
        let boxes = colored_boxes(
            "<table style='width:40px;table-layout:fixed;border-spacing:2px'><tr><td rowspan='2' style='height:10px;background:red'></td><td style='height:4px;background:green'></td></tr><tr><td style='height:4px;background:red'></td></tr><tr><td colspan='2' style='height:6px;background:green'></td></tr></table>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (2.0, 2.0, 17.0, 10.0),
                (21.0, 2.0, 17.0, 4.0),
                (21.0, 8.0, 17.0, 4.0),
                (2.0, 14.0, 36.0, 6.0)
            ]
        );
    }

    #[test]
    fn fixed_table_uses_horizontal_and_vertical_border_spacing() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}table{width:260px;table-layout:fixed;border-spacing:8px 6px;background:#e8eef8}td{height:20px;padding:0}tr:first-child td:first-child{width:40px;background:#174ea6}tr:first-child td:nth-child(2){height:32px;background:#fbbc04}tr:first-child td:last-child{background:#ea4335}tr:last-child td:first-child{background:#34a853}tr:last-child td:nth-child(2){background:#a142f4}tr:last-child td:last-child{background:#00acc1}</style><table><tbody><tr><td></td><td></td><td></td></tr><tr><td></td><td></td><td></td></tr></tbody></table>",
            64,
        )
        .unwrap();
        let list = display_list(&document, 320, 200, &FixedText).unwrap();
        let rect_for = |rgb| {
            list.0.iter().find_map(|command| match command {
                Command::FillRect { rect, color } if (color.r, color.g, color.b) == rgb => {
                    Some(*rect)
                }
                _ => None,
            })
        };
        assert_eq!(
            rect_for((232, 238, 248)),
            Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 260.0,
                height: 70.0
            })
        );
        assert_eq!(
            rect_for((23, 78, 166)),
            Some(Rect {
                x: 8.0,
                y: 6.0,
                width: 40.0,
                height: 32.0
            })
        );
        assert_eq!(
            rect_for((251, 188, 4)),
            Some(Rect {
                x: 56.0,
                y: 6.0,
                width: 94.0,
                height: 32.0
            })
        );
        assert_eq!(
            rect_for((234, 67, 53)),
            Some(Rect {
                x: 158.0,
                y: 6.0,
                width: 94.0,
                height: 32.0
            })
        );
        assert_eq!(
            rect_for((52, 168, 83)),
            Some(Rect {
                x: 8.0,
                y: 44.0,
                width: 40.0,
                height: 20.0
            })
        );
        assert_eq!(
            rect_for((161, 66, 244)),
            Some(Rect {
                x: 56.0,
                y: 44.0,
                width: 94.0,
                height: 20.0
            })
        );
        assert_eq!(
            rect_for((0, 172, 193)),
            Some(Rect {
                x: 158.0,
                y: 44.0,
                width: 94.0,
                height: 20.0
            })
        );
    }

    #[test]
    fn fixed_table_first_row_percentages_size_columns_from_track_area() {
        let document = crate::html::parse(
            "<style>body{margin:0}td{height:26px;padding:0}</style><table style='width:240px;table-layout:fixed;border-spacing:6px'><tr><td style='width:25%;background:#ff0000'></td><td style='width:75%;background:#00ff00'></td></tr><tr><td style='background:#0000ff'></td><td style='background:#ffff00'></td></tr></table>",
            64,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let table = crate::selector::query_selector(&document, document.root(), "table")
            .unwrap()
            .unwrap();
        let cell = crate::selector::query_selector(&document, document.root(), "td")
            .unwrap()
            .unwrap();
        let table_style = css::compute_node(&document, table, None, &rules).unwrap();
        let cell_style = css::compute_node(&document, cell, Some(&table_style), &rules).unwrap();
        assert_eq!(table_style.width, Some(240.0));
        assert!(table_style.table_fixed);
        assert_eq!(
            cell_style.resolve_percentages(222.0, None).width,
            Some(55.5)
        );
        let list = display_list(&document, 320, 200, &FixedText).unwrap();
        let cells = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color }
                    if color.a == 255
                        && matches!(
                            (color.r, color.g, color.b),
                            (255, 0, 0) | (0, 255, 0) | (0, 0, 255) | (255, 255, 0)
                        ) =>
                {
                    Some((color.r, color.g, color.b, *rect))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let green = cells
            .iter()
            .find(|(r, g, b, _)| (*r, *g, *b) == (0, 255, 0))
            .unwrap()
            .3;
        let red = cells
            .iter()
            .find(|(r, g, b, _)| (*r, *g, *b) == (255, 0, 0))
            .unwrap()
            .3;
        let blue = cells
            .iter()
            .find(|(r, g, b, _)| (*r, *g, *b) == (0, 0, 255))
            .unwrap()
            .3;
        let yellow = cells
            .iter()
            .find(|(r, g, b, _)| (*r, *g, *b) == (255, 255, 0))
            .unwrap()
            .3;
        for rect in [red, blue] {
            assert!((rect.x - 6.0).abs() < 0.001);
            assert!(
                (rect.width - 55.5).abs() < 0.001,
                "percentage column rect: {rect:?}"
            );
        }
        for rect in [green, yellow] {
            assert!((rect.x - 67.5).abs() < 0.001);
            assert!(
                (rect.width - 166.5).abs() < 0.001,
                "percentage column rect: {rect:?}"
            );
        }
    }

    #[test]
    fn table_height_expands_rows_after_intrinsic_row_sizing() {
        let boxes = colored_boxes(
            "<style>td{padding:0;background:red}div{height:10px}</style><table style='width:80px;height:60px;table-layout:fixed;border-spacing:0'><tr><td><div></div></td></tr><tr><td><div></div></td></tr></table>",
        );
        assert_eq!(boxes.len(), 2, "row backgrounds: {boxes:?}");
        assert!(
            boxes
                .iter()
                .all(|rect| (rect.width - 80.0).abs() < 0.01 && (rect.height - 30.0).abs() < 0.01),
            "the extra table height is distributed evenly across rows: {boxes:?}"
        );
    }

    #[test]
    fn auto_table_spans_consume_spacing_without_expanding_empty_tracks() {
        let boxes = colored_boxes(
            "<style>td{background:red;padding:0}</style><table style='width:240px;border-spacing:4px;line-height:0'><caption><div style='height:10px'></div></caption><tr><td colspan='2'><div style='width:60px;height:12px'></div></td><td><div style='width:20px;height:12px'></div></td></tr><tr><td rowspan='2'><div style='width:30px;height:12px'></div></td><td><div style='height:12px'></div></td><td><div style='height:12px'></div></td></tr><tr><td><div style='height:12px'></div></td><td><div style='height:12px'></div></td></tr></table>",
        );
        let first_span = boxes
            .iter()
            .find(|rect| (rect.y - 14.0).abs() < 0.01 && rect.width > 100.0)
            .expect("first-row spanning cell background");
        assert!(
            (first_span.width - 169.05).abs() < 0.02,
            "the colspan track includes one internal spacing: {first_span:?}"
        );
        let row_span = boxes
            .iter()
            .find(|rect| (rect.y - 30.0).abs() < 0.01 && rect.width > 100.0)
            .expect("row-spanning cell background");
        assert!(
            (row_span.width - 165.05).abs() < 0.02 && (row_span.height - 28.0).abs() < 0.01,
            "the occupied first track grows without assigning width to its empty neighbor: {row_span:?}"
        );
        // The two painted span extents pin the occupied-track sizing. The
        // table Chrome fixture separately covers placement of later tracks.
    }

    #[test]
    fn collapsed_table_borders_choose_width_and_origin_winners() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}table{border-collapse:collapse;border:4px solid #174ea6}td{width:42px;height:28px;padding:0;border:4px solid #ea4335}td+td{border-left:8px solid #fbbc04}</style><table><tr><td></td><td></td></tr><tr><td></td><td></td></tr></table>",
            32,
        )
        .unwrap();
        let list = display_list(&document, 160, 100, &FixedText).unwrap();
        let fills = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.a > 0 => Some((*rect, *color)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            fills.iter().any(|(rect, color)| {
                (color.r, color.g, color.b) == (251, 188, 4)
                    && (rect.width - 8.0).abs() < 0.01
                    && rect.height > 20.0
            }),
            "the wider adjacent cell border wins: {fills:?}"
        );
        assert!(
            fills.iter().any(|(rect, color)| {
                (color.r, color.g, color.b) == (234, 67, 53)
                    && (rect.height - 4.0).abs() < 0.01
                    && rect.width > 30.0
            }),
            "the cell border wins the equal-width table border: {fills:?}"
        );
        assert!(
            !fills
                .iter()
                .any(|(_, color)| (color.r, color.g, color.b) == (23, 78, 166))
        );
    }

    #[test]
    fn collapsed_table_width_counts_outer_halves_once() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}table{border-collapse:collapse;border:4px solid blue}td{width:42px;height:28px;padding:0;border:4px solid red}td+td{border-left:8px solid #fbbc04}</style><table><tr><td></td><td></td></tr><tr><td></td><td></td></tr></table>",
            32,
        )
        .unwrap();
        let list = display_list(&document, 160, 100, &FixedText).unwrap();
        let borders = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color }
                    if color.a > 0
                        && matches!((color.r, color.g, color.b), (255, 0, 0) | (251, 188, 4)) =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!borders.is_empty());
        let min_x = borders
            .iter()
            .map(|rect| rect.x)
            .fold(f32::INFINITY, f32::min);
        let min_y = borders
            .iter()
            .map(|rect| rect.y)
            .fold(f32::INFINITY, f32::min);
        let max_x = borders
            .iter()
            .map(|rect| rect.x + rect.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = borders
            .iter()
            .map(|rect| rect.y + rect.height)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (min_x - 0.0).abs() < 0.01 && (max_x - 100.0).abs() < 0.01,
            "two 42px cells plus 4/8/4px collapsed edges occupy 100px: {borders:?}"
        );
        assert!(
            (min_y - 0.0).abs() < 0.01 && (max_y - 68.0).abs() < 0.01,
            "two 28px cells plus 4px collapsed row edges occupy 68px: {borders:?}"
        );
    }

    #[test]
    fn collapsed_hidden_border_suppresses_the_adjacent_visible_edge() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}table{border-collapse:collapse}td{width:20px;height:12px;padding:0;border:4px solid red}td+td{border-left:20px hidden blue}</style><table><tr><td></td><td></td></tr></table>",
            32,
        )
        .unwrap();
        let list = display_list(&document, 120, 50, &FixedText).unwrap();
        let vertical = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color }
                    if color.a > 0
                        && (color.r, color.g, color.b) == (255, 0, 0)
                        && rect.height > 6.0
                        && rect.width <= 4.01 =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!vertical.is_empty());
        let min_x = vertical
            .iter()
            .map(|rect| rect.x)
            .fold(f32::INFINITY, f32::min);
        let max_x = vertical
            .iter()
            .map(|rect| rect.x + rect.width)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            vertical.iter().all(|rect| (rect.x - min_x).abs() < 0.01
                || ((rect.x + rect.width) - max_x).abs() < 0.01),
            "the hidden 20px border wins and paints no internal vertical edge: {vertical:?}"
        );
    }

    #[test]
    fn collapsed_border_styles_follow_css_conflict_precedence() {
        let candidate = |style, width, origin_rank| CollapsedBorderCandidate {
            width,
            color: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            style,
            origin_rank,
            tie_order: 0,
            prefer_larger_tie: false,
        };
        assert!(collapsed_border_wins(
            candidate(BorderStyle::Double, 4.0, 6),
            candidate(BorderStyle::Solid, 4.0, 6),
        ));
        assert!(collapsed_border_wins(
            candidate(BorderStyle::Solid, 5.0, 6),
            candidate(BorderStyle::Double, 4.0, 6),
        ));
        assert!(collapsed_border_wins(
            candidate(BorderStyle::Hidden, 0.0, 1),
            candidate(BorderStyle::Double, 100.0, 6),
        ));
        assert!(collapsed_border_wins(
            candidate(BorderStyle::Solid, 4.0, 6),
            candidate(BorderStyle::Solid, 4.0, 1),
        ));
    }

    #[test]
    fn css_table_roles_use_same_geometry() {
        let boxes = colored_boxes(
            "<div style='display:table;width:30px;table-layout:fixed;border-spacing:0px'><div style='display:table-row'><div style='display:table-cell;height:6px;background:red'></div><div style='display:table-cell;height:10px;background:green'></div></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 15.0, 10.0), (15.0, 0.0, 15.0, 10.0)]
        );
    }

    #[test]
    fn table_column_limit_rejects_overflow() {
        let document = crate::html::parse(
            "<table><tr><td colspan='256'></td><td></td></tr></table>",
            32,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 100, 100, &FixedText),
            Err(LayoutError::CommandLimit)
        );
    }

    fn colored_boxes(markup: &str) -> Vec<Rect> {
        colored_boxes_in(markup, 100, 100)
    }

    fn colored_boxes_in(markup: &str, width: u32, height: u32) -> Vec<Rect> {
        let markup = alloc::format!("<style>body{{margin:0}}</style>{markup}");
        let document = crate::html::parse(&markup, 32).unwrap();
        display_list(&document, width, height, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.a == 255 && color.b == 0 => Some(rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn later_relative_flex_item_paints_above_earlier_positioned_siblings() {
        let document = crate::html::parse(
            "<style>body{margin:0}#flex{display:flex;width:100px;height:100px}#flex>div{width:50px;height:50px;background:green;position:relative;top:50px;left:50px}#flex>div>div{width:50px;height:50px;position:absolute;left:100%;top:100%}</style><div style='position:absolute;background:red;width:50px;height:50px;left:50px;top:50px'></div><div style='position:absolute;background:red;width:50px;height:50px;left:100px;top:100px'></div><div id='flex'><div><div style='background:green'></div></div></div>",
            32,
        )
        .unwrap();
        let paints: Vec<_> = display_list(&document, 200, 200, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color }
                    if (color.r, color.g, color.b) == (255, 0, 0)
                        || (color.r, color.g, color.b) == (0, 128, 0) =>
                {
                    Some(((color.r, color.g, color.b), (rect.x, rect.y)))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            paints,
            [
                ((255, 0, 0), (50.0, 50.0)),
                ((255, 0, 0), (100.0, 100.0)),
                ((0, 128, 0), (50.0, 50.0)),
                ((0, 128, 0), (100.0, 100.0)),
            ]
        );
    }

    fn red_green_paints(markup: &str) -> Vec<((u8, u8, u8), Rect)> {
        let markup = alloc::format!("<style>body{{margin:0}}</style>{markup}");
        let document = crate::html::parse(&markup, 32).unwrap();
        display_list(&document, 100, 100, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color }
                    if (color.r, color.g, color.b) == (255, 0, 0)
                        || (color.r, color.g, color.b) == (0, 128, 0) =>
                {
                    Some(((color.r, color.g, color.b), rect))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn absolute_flex_children_paint_over_in_flow_items() {
        let paints = red_green_paints(
            "<main style='display:flex;width:20px;height:20px'><div style='width:20px;height:20px;background:green'></div><div style='position:absolute;left:0;top:0;width:20px;height:20px;background:red'></div></main>",
        );
        assert_eq!(
            paints,
            [
                (
                    (0, 128, 0),
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 20.0,
                        height: 20.0
                    }
                ),
                (
                    (255, 0, 0),
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 20.0,
                        height: 20.0
                    }
                ),
            ]
        );
    }

    #[test]
    fn absolute_grid_children_paint_over_in_flow_items() {
        let paints = red_green_paints(
            "<main style='display:grid;width:20px;grid-template-columns:20px;grid-template-rows:20px'><div style='background:green'></div><div style='position:absolute;left:0;top:0;width:20px;height:20px;background:red'></div></main>",
        );
        assert_eq!(
            paints,
            [
                (
                    (0, 128, 0),
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 20.0,
                        height: 20.0
                    }
                ),
                (
                    (255, 0, 0),
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 20.0,
                        height: 20.0
                    }
                ),
            ]
        );
    }

    #[test]
    fn rtl_blocks_resolve_overflow_and_physical_auto_margins() {
        let boxes = colored_boxes(
            "<style>body{margin:0}main{direction:rtl;width:80px}div{height:5px;background:red}</style><main><div style='width:30px;margin-left:20px'></div><div style='width:120px'></div><div style='width:30px;margin:auto'></div><div style='width:30px;margin-right:auto'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
            [50.0, -40.0, 25.0, 0.0]
        );
    }

    #[test]
    fn rtl_flex_row_reverse_and_column_start_follow_inline_direction() {
        for (direction, expected) in [
            ("row", [60.0, 40.0]),
            ("row-reverse", [0.0, 20.0]),
            ("column", [60.0, 60.0]),
        ] {
            let markup = alloc::format!(
                "<style>body{{margin:0}}main{{display:flex;width:80px;direction:rtl;flex-direction:{direction};align-items:start}}div{{width:20px;height:5px;background:red}}</style><main><div></div><div></div></main>"
            );
            let boxes = colored_boxes(&markup);
            assert_eq!(
                boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
                expected,
                "{direction}"
            );
        }
    }

    #[test]
    fn rtl_table_columns_and_cell_block_content_start_on_the_right() {
        let boxes = colored_boxes(
            "<style>body{margin:0}table{direction:rtl;width:80px;border-spacing:0}td{padding:0}div{width:20px;height:5px;background:red}</style><table><tr><td><div></div></td><td><div></div></td></tr></table>",
        );
        assert_eq!(
            boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
            [60.0, 20.0]
        );
    }

    #[test]
    fn width_constraints_apply_to_auto_explicit_and_border_box_widths() {
        let boxes = colored_boxes(
            "<div style='width:10px;min-width:20px;max-width:5px;height:4px;background:red'></div><div style='max-width:12px;height:4px;background:green'></div><div style='box-sizing:border-box;min-width:20px;width:8px;padding:2px;height:8px;background:red'></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| r.width).collect::<Vec<_>>(),
            [20.0, 12.0, 20.0]
        );
    }

    #[test]
    fn grid_fraction_tracks_gaps_spans_and_sparse_auto_placement() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:20px 1fr 2fr;grid-template-rows:10px 20px;gap:5px'><div style='grid-column:2 / span 2;background:red'></div><div style='background:green'></div><div style='background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (25.0, 0.0, 75.0, 10.0),
                (0.0, 15.0, 20.0, 20.0),
                (25.0, 15.0, 70.0 / 3.0, 20.0)
            ]
        );
    }

    #[test]
    fn grid_browser_fixture_has_chrome_integral_geometry() {
        let boxes = colored_boxes(include_str!("../tests/grid-browser.html"));
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (25.0, 0.0, 65.0, 10.0),
                (0.0, 15.0, 20.0, 20.0),
                (25.0, 15.0, 20.0, 20.0)
            ]
        );
    }

    #[test]
    fn grid_bounded_dense_document_metrics() {
        extern crate std;
        let columns = "1fr ".repeat(32);
        let markup = alloc::format!(
            "<div style='display:grid;width:320px;grid-template-columns:{columns}'>{}</div>",
            "<div style='height:1px;background:red'></div>".repeat(1024)
        );
        let document = crate::html::parse(&markup, 1100).unwrap();
        let started = std::time::Instant::now();
        let list = display_list(&document, 320, 64, &FixedText).unwrap();
        assert_eq!(list.0.iter().filter(|command| matches!(command, Command::FillRect { color, .. } if color.r == 255 && color.g == 0 && color.b == 0 && color.a == 255)).count(), 1024);
        assert!(core::mem::size_of::<Style>() <= 256);
        std::println!(
            "grid: 1024 items, {} commands, {} style bytes, 512 occupancy bytes, {:?} layout",
            list.0.len(),
            core::mem::size_of::<Style>(),
            started.elapsed()
        );
    }

    #[test]
    fn grid_auto_rows_measure_wrapped_content_at_final_column_width() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:8px;grid-template-columns:8px'><div style='background:red;font-size:10px;line-height:10px'>aa aa</div><div style='background:green;height:3px'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>(),
            [(0.0, 20.0), (20.0, 3.0)]
        );
    }

    #[test]
    fn grid_text_nodes_generate_anonymous_items_in_auto_placement_order() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}</style><main style='display:grid;width:40px;grid-template-columns:20px 20px;grid-template-rows:10px 10px;font-size:10px;line-height:10px'>A<div style='height:10px;background:red'></div>B</main>",
            64,
        )
        .unwrap();
        let list = display_list(&document, 40, 30, &FixedText).unwrap();
        let glyph_runs = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(glyph_runs.len(), 2);
        assert_eq!(glyph_runs[0].0, 0.0);
        assert_eq!(glyph_runs[1].0, 0.0);
        assert_eq!(glyph_runs[1].1 - glyph_runs[0].1, 10.0);
    }

    #[test]
    fn subgrid_text_items_contribute_to_inherited_row_sizing() {
        let boxes = colored_boxes(
            "<style>html,body{margin:0}</style><main style='display:grid;width:20px;grid-template-columns:20px;grid-template-rows:auto auto'><section style='display:grid;grid-row:1;grid-template-columns:20px;grid-template-rows:subgrid;font-size:10px;line-height:10px'>A</section><div style='height:3px;background:green'></div></main>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 10.0, 3.0)]
        );
    }

    #[test]
    fn grid_indefinite_fraction_rows_share_one_fraction_size() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:10px;grid-template-rows:1fr 2fr'><div style='height:10px;background:red'></div><div style='height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>(),
            [(0.0, 10.0), (10.0, 6.0)]
        );
    }

    #[test]
    fn grid_definite_items_reserve_space_before_auto_items() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:40px;grid-template-columns:20px 20px;grid-template-rows:10px 10px'><div style='background:red'></div><div style='grid-column:1;grid-row:1;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(20.0, 0.0, 20.0, 10.0), (0.0, 0.0, 20.0, 10.0)]
        );
    }

    #[test]
    fn grid_auto_tracks_and_implicit_rows_use_item_intrinsics() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:50px;grid-template-columns:auto 20px;gap:2px'><div style='height:8px;background:red'></div><div style='height:4px;background:green'></div><div style='height:6px;background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 0.0, 28.0, 8.0),
                (30.0, 0.0, 20.0, 4.0),
                (0.0, 10.0, 28.0, 6.0)
            ]
        );
    }

    fn fill_colors(list: &DisplayList) -> Vec<Rgba> {
        list.0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { color, .. } if color.a == 255 => Some(*color),
                _ => None,
            })
            .collect()
    }

    fn has_color(colors: &[Rgba], r: u8, g: u8, b: u8) -> bool {
        colors
            .iter()
            .any(|color| (color.r, color.g, color.b) == (r, g, b))
    }

    #[test]
    fn z_index_orders_positioned_paint_and_negative_escape() {
        // Positive z-index paints after auto; a negative z-index escapes below
        // the backgrounds of non-stacking-context ancestors.
        let doc = crate::html::parse(
            "<style>html,body{margin:0;background:#fff}.wrap{width:100px;height:50px;background:blue}.neg{position:absolute;z-index:-1;left:0;top:0;width:40px;height:40px;background:red}.top{position:absolute;left:0;top:0;width:30px;height:30px;background:green;z-index:2}.auto{position:absolute;left:0;top:0;width:20px;height:20px;background:yellow}</style><div class=wrap><div class=neg></div></div><div class=top></div><div class=auto></div>",
            64,
        )
        .unwrap();
        let list = display_list(&doc, 200, 200, &FixedText).unwrap();
        let index_of = |r: u8, g: u8, b: u8| {
            list.0.iter().position(|command| matches!(command, Command::FillRect { color, .. } if (color.r, color.g, color.b) == (r, g, b)))
        };
        let red = index_of(255, 0, 0).expect("red painted");
        let blue = index_of(0, 0, 255).expect("blue painted");
        let yellow = index_of(255, 255, 0).expect("yellow painted");
        let green = index_of(0, 128, 0).expect("green painted");
        assert!(
            red < blue,
            "negative z-index paints below ancestor backgrounds"
        );
        assert!(green > yellow, "positive z-index paints above auto");
    }

    #[test]
    fn z_index_zero_creates_a_stacking_context_above_negative_children() {
        let doc = crate::html::parse(
            "<style>html,body{margin:0;background:#fff}main{position:relative;z-index:0;background:none}.neg{position:absolute;z-index:-1;left:20px;top:20px;width:100px;height:100px;background:red}.flow{width:160px;height:60px;background:green}</style><main><div class=neg></div><div class=flow></div></main>",
            64,
        )
        .unwrap();
        let list = display_list(&doc, 220, 160, &FixedText).unwrap();
        let index_of = |r: u8, g: u8, b: u8| {
            list.0.iter().position(|command| matches!(command, Command::FillRect { color, .. } if (color.r, color.g, color.b) == (r, g, b)))
        };
        let red = index_of(255, 0, 0).expect("red painted");
        let green = index_of(0, 128, 0).expect("green painted");
        assert!(red < green, "negative child paints below in-flow content");
    }

    #[test]
    fn aspect_ratio_derives_the_auto_dimension() {
        let doc = crate::html::parse(
            "<style>body{margin:0}div{aspect-ratio:2/1;width:120px;background:red}span{display:block;aspect-ratio:1/2;height:40px;background:blue}</style><div></div><span></span>",
            32,
        )
        .unwrap();
        let list = display_list(&doc, 200, 200, &FixedText).unwrap();
        let rects: Vec<Rect> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.a == 255 => Some(*rect),
                _ => None,
            })
            .collect();
        assert!(
            rects
                .iter()
                .any(|rect| rect.width == 120.0 && rect.height == 60.0)
        );
        assert!(
            rects
                .iter()
                .any(|rect| rect.width == 20.0 && rect.height == 40.0)
        );
    }

    #[test]
    fn shadow_roots_render_instead_of_light_children_with_scoped_styles() {
        // The document sheet styles `b` blue; the shadow sheet styles `i` red.
        // The host composes: the light `b` is not rendered at all and the
        // document rule must not reach the shadow `i`.
        let mut document =
            crate::html::parse("<style>b{background:blue}</style><div><b></b></div>", 32).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let root = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let shadow =
            crate::html::parse_fragment(&mut document, "<style>i{background:red}</style><i></i>")
                .unwrap();
        document.append(root, shadow).unwrap();
        let list = display_list(&document, 100, 100, &FixedText).unwrap();
        let colors = fill_colors(&list);
        assert!(has_color(&colors, 255, 0, 0), "shadow i renders red");
        assert!(!has_color(&colors, 0, 0, 255), "light b is not rendered");
    }

    #[test]
    fn host_rules_style_the_host_but_plain_shadow_rules_do_not() {
        let mut document = crate::html::parse("<div></div>", 32).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let root = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let shadow = crate::html::parse_fragment(
            &mut document,
            "<style>:host{background:red}div{background:blue}</style><span style='background:green'></span>",
        )
        .unwrap();
        document.append(root, shadow).unwrap();
        let list = display_list(&document, 100, 100, &FixedText).unwrap();
        let colors = fill_colors(&list);
        assert!(has_color(&colors, 255, 0, 0), ":host paints the host red");
        assert!(has_color(&colors, 0, 128, 0), "shadow span paints green");
        assert!(
            !has_color(&colors, 0, 0, 255),
            "plain shadow rules must not match the host"
        );
    }

    #[test]
    fn slotted_rules_style_assigned_light_children_only() {
        let mut document =
            crate::html::parse("<div><b></b><i style='background:blue'></i></div>", 32).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let root = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let shadow = crate::html::parse_fragment(
            &mut document,
            "<style>::slotted(b){background:red}</style><slot></slot>",
        )
        .unwrap();
        document.append(root, shadow).unwrap();
        let list = display_list(&document, 100, 100, &FixedText).unwrap();
        let colors = fill_colors(&list);
        assert!(has_color(&colors, 255, 0, 0), "::slotted(b) paints b red");
        assert!(
            has_color(&colors, 0, 0, 255),
            "slotted i keeps its inline blue; ::slotted(b) does not style it"
        );
    }

    #[test]
    fn slotted_content_inherits_from_the_slot() {
        let mut document = crate::html::parse("<div><b>text</b></div>", 32).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let root = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let shadow = crate::html::parse_fragment(
            &mut document,
            "<style>slot{color:rgb(0,128,0)}</style><slot></slot>",
        )
        .unwrap();
        document.append(root, shadow).unwrap();
        let list = display_list(&document, 100, 100, &FixedText).unwrap();
        let green = list.0.iter().any(|command| {
            matches!(
                command,
                Command::GlyphRun { color, .. }
                    if (color.r, color.g, color.b) == (0, 128, 0)
            )
        });
        assert!(green, "slotted text inherits the slot color");
    }

    #[test]
    fn parts_reach_shadow_content_from_document_styles() {
        let mut document = crate::html::parse(
            "<style>::part(label){background:red}</style><div></div>",
            32,
        )
        .unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let root = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let shadow = crate::html::parse_fragment(
            &mut document,
            "<div part='label'></div><p part='other'></p>",
        )
        .unwrap();
        document.append(root, shadow).unwrap();
        let list = display_list(&document, 100, 100, &FixedText).unwrap();
        assert!(
            has_color(&fill_colors(&list), 255, 0, 0),
            "::part(label) paints the exposing element"
        );
        // Scoped pseudo selectors stay inert outside their scope.
        assert!(!crate::selector::matches(&document, host, ":host").unwrap());
    }

    fn grid_boxes(markup: &str) -> Vec<Rect> {
        let document = crate::html::parse(markup, 32).unwrap();
        grid_boxes_at(&document, 100, 100)
    }

    fn grid_boxes_at(document: &Document, width: u32, height: u32) -> Vec<Rect> {
        display_list(document, width, height, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.a == 255 && color.g == 0 => Some(rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn grid_auto_fill_expands_to_fit_the_available_size() {
        // minmax(30px, 1fr): the 1fr max is indefinite for expansion, so the
        // 30px minimum drives floor((100+10)/(30+10)) = 2 repetitions.
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:100px;gap:10px;grid-template-columns:repeat(auto-fill,minmax(30px,1fr))'><div style='height:5px;background:red'></div><div style='height:5px;background:blue'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 45.0), (55.0, 45.0)]
        );
    }

    #[test]
    fn grid_auto_repeat_expands_prefix_suffix_and_complete_track_patterns() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><main style='display:grid;width:120px;gap:5px;grid-template-columns:10px repeat(auto-fill,20px 15px) 5px'><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div></main>",
            32,
        )
        .unwrap();
        let boxes = grid_boxes_at(&document, 160, 100);
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [
                (0.0, 10.0),
                (15.0, 20.0),
                (40.0, 15.0),
                (60.0, 20.0),
                (85.0, 15.0),
                (105.0, 5.0),
            ]
        );
    }

    #[test]
    fn grid_auto_repeat_names_resolve_across_repetitions() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:120px;gap:5px;grid-template-columns:repeat(2,5px) [outer] repeat(auto-fill,[auto-line] 20px [auto-end]) repeat(2,5px) [last]'><div style='grid-column:auto-line 2 / auto-end 3;height:4px;background:red'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(45.0, 45.0)]
        );
    }

    #[test]
    fn grid_auto_repeat_expands_fixed_prefix_and_suffix_repeats() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><main style='display:grid;width:120px;gap:5px;grid-template-columns:repeat(2,5px) [outer] repeat(auto-fill,20px) repeat(2,5px) [last]'><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div><div style='height:4px;background:red'></div></main>",
            32,
        )
        .unwrap();
        let boxes = grid_boxes_at(&document, 160, 100);
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [
                (0.0, 5.0),
                (10.0, 5.0),
                (20.0, 20.0),
                (45.0, 20.0),
                (70.0, 20.0),
                (95.0, 5.0),
                (105.0, 5.0),
            ]
        );
    }

    #[test]
    fn grid_auto_repeat_row_patterns_use_definite_block_space() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:20px;height:65px;gap:5px;grid-template-columns:20px;grid-template-rows:5px repeat(auto-fill,10px 5px) 5px'><div style='background:red'></div><div style='background:red'></div><div style='background:red'></div><div style='background:red'></div><div style='background:red'></div><div style='background:red'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>(),
            [
                (0.0, 5.0),
                (10.0, 10.0),
                (25.0, 5.0),
                (35.0, 10.0),
                (50.0, 5.0),
                (60.0, 5.0),
            ]
        );
    }

    #[test]
    fn grid_subgrid_inherits_auto_repeat_named_lines() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:120px;gap:5px;grid-template-columns:10px [outer] repeat(auto-fill,[inner-start] 20px [inner-end]) 5px [last]'><section style='display:grid;grid-column:outer / last;grid-template-columns:subgrid;height:8px'><div style='grid-column:inner-start 2 / inner-end 3;height:4px;background:red'></div></section></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(40.0, 45.0)]
        );
    }

    #[test]
    fn grid_subgrid_inherits_names_across_fixed_repeat_prefix_and_suffix() {
        let document = crate::html::parse(
            include_str!("../../lumen-html-image/tests/render/jixr/grid-auto-repeat-prefix-named-subgrid.html"),
            32,
        )
        .unwrap();
        let rules = stylesheets(&document).unwrap();
        let main = crate::selector::query_selector(&document, document.root(), "main")
            .unwrap()
            .unwrap();
        let main_style = css::compute_node(&document, main, None, &rules).unwrap();
        let auto = main_style.grid_columns_auto.as_ref().unwrap();
        assert_eq!(auto.prefix_tracks.len(), 2);
        assert_eq!(auto.suffix_tracks.len(), 2);
        assert_eq!(auto.prefix_names[0].name.as_ref(), "outer");
        assert_eq!(auto.prefix_names[0].line, 2);
        assert_eq!(auto.suffix_names[0].name.as_ref(), "last");
        assert_eq!(auto.suffix_names[0].line, 2);
        assert_eq!(auto.repeat_names.len(), 2);
        let section = crate::selector::query_selector(&document, document.root(), "section")
            .unwrap()
            .unwrap();
        let section_style =
            css::compute_node(&document, section, Some(&main_style), &rules).unwrap();
        assert!(section_style.grid_columns_subgrid);
        let cell = crate::selector::query_selector(&document, document.root(), ".cell")
            .unwrap()
            .unwrap();
        let cell_style = css::compute_node(&document, cell, Some(&section_style), &rules).unwrap();
        assert!(
            cell_style
                .grid_column_spec
                .as_deref()
                .is_some_and(|raw| raw.contains("auto-line 2") && raw.contains("auto-end 3")),
            "unexpected computed grid-column source: {:?}",
            cell_style.grid_column_spec
        );
        let boxes = grid_boxes_at(&document, 320, 200);
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(45.0, 0.0, 45.0, 4.0)],
            "the fixture's inherited repeated-line placement should match Chrome"
        );
    }

    #[test]
    fn grid_fit_content_percentage_caps_max_content_tracks() {
        let boxes = colored_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:100px;grid-template-columns:fit-content(25%) auto'><div style='height:4px;background:red'>aa bb cc dd ee ff</div></main>",
        );
        assert_eq!(boxes.len(), 1);
        assert!(
            (boxes[0].width - 25.0).abs() < 0.01,
            "fit-content(25%) track should cap at 25px, got {:?}",
            boxes[0]
        );
    }

    #[test]
    fn grid_auto_repeat_respects_the_total_explicit_track_bound() {
        let document = crate::html::parse(
            "<main style='display:grid;width:100px;grid-template-columns:repeat(64,1px) repeat(auto-fill,1px)'></main>",
            32,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 100, 100, &FixedText),
            Err(LayoutError::GridLimit)
        );

        // A wide definite box whose auto-fill would need more than the
        // remaining explicit-track budget must fail instead of truncating the
        // repeat count at the implementation limit.
        let document = crate::html::parse(
            "<style>body{margin:0}</style><main style='display:grid;width:1000px;grid-template-columns:repeat(auto-fill,1px)'></main>",
            32,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 1000, 100, &FixedText),
            Err(LayoutError::GridLimit)
        );
    }

    #[test]
    fn grid_auto_repeat_uses_the_ua_floor_for_zero_sized_repeat_tracks() {
        let auto = css::GridAutoRepeat {
            fit: false,
            tracks: Arc::from([css::GridTrack::Pixels(0.0)]),
            prefix_tracks: Vec::new().into(),
            suffix_tracks: Vec::new().into(),
            prefix_names: Vec::<css::GridNamedLine>::new().into(),
            repeat_names: Vec::<css::GridNamedLine>::new().into(),
            suffix_names: Vec::<css::GridNamedLine>::new().into(),
        };
        assert_eq!(auto_repeat_count(&auto, Some(20.0), 0.0), Ok(20));
        assert_eq!(auto_repeat_count(&auto, Some(20.0), 2.0), Ok(7));
        // An overflowing first repetition still yields one repetition per
        // §7.2.3.2, while a count beyond the bounded explicit grid errors.
        assert_eq!(auto_repeat_count(&auto, Some(0.25), 0.0), Ok(1));
        assert_eq!(
            auto_repeat_count(&auto, Some((css::MAX_GRID_TRACKS + 1) as f32), 0.0),
            Err(LayoutError::GridLimit)
        );
    }

    #[test]
    fn grid_auto_fit_collapses_empty_repeated_tracks() {
        // Four repetitions fit 160px; auto-fill keeps the two empty tracks.
        let fill = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:160px;gap:10px;grid-template-columns:repeat(auto-fill,minmax(30px,1fr))'><div style='height:5px;background:red'></div><div style='height:5px;background:blue'></div></main>",
        );
        assert_eq!(
            fill.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 32.5), (42.5, 32.5)]
        );
        // auto-fit collapses them and the 1fr columns absorb the space.
        let fit = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:160px;gap:10px;grid-template-columns:repeat(auto-fit,minmax(30px,1fr))'><div style='height:5px;background:red'></div><div style='height:5px;background:blue'></div></main>",
        );
        assert_eq!(
            fit.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 75.0), (85.0, 75.0)]
        );
    }

    #[test]
    fn grid_auto_fit_keeps_fixed_sides_and_collapses_only_repeat_tracks() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:160px;gap:10px;grid-template-columns:10px [outer] repeat(auto-fit,minmax(30px,1fr)) 5px [last]'><div style='grid-column:2;height:4px;background:red'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(20.0, 125.0)]
        );
    }

    #[test]
    fn grid_auto_fit_rebases_named_lines_after_collapsing_tracks() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:160px;gap:10px;grid-template-columns:10px [outer] repeat(auto-fit,[cell] minmax(30px,1fr)) 5px [last]'><div style='grid-column:cell 2;height:4px;background:red'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(20.0, 125.0)]
        );
    }

    #[test]
    fn grid_auto_fill_rows_need_a_definite_block_size() {
        // An indefinite container height yields one explicit repetition; the
        // remaining rows are implicit auto tracks.
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:20px;grid-template-columns:20px;grid-template-rows:repeat(auto-fill,10px)'><div style='height:5px;background:red'></div><div style='height:5px;background:red'></div><div style='height:5px;background:red'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|r| r.y).collect::<Vec<_>>(),
            [0.0, 10.0, 15.0]
        );
    }

    #[test]
    fn grid_template_shorthand_sets_rows_and_columns() {
        let boxes = grid_boxes(
            "<style>body{margin:0}</style><main style='display:grid;width:40px;grid-template:10px 20px / 20px 20px'><div style='background:red'></div><div style='background:red'></div><div style='background:red'></div></main>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 0.0, 20.0, 10.0),
                (20.0, 0.0, 20.0, 10.0),
                (0.0, 10.0, 20.0, 20.0)
            ]
        );
    }

    #[test]
    fn grid_rejects_placement_beyond_bounded_tracks() {
        let document = crate::html::parse(
            "<div style='display:grid'><div style='grid-column:64 / span 2'></div></div>",
            8,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 100, 100, &FixedText),
            Err(LayoutError::GridLimit)
        );
    }

    #[test]
    fn flex_width_constraints_freeze_and_redistribute_free_space() {
        let grow = colored_boxes(
            "<div style='display:flex;width:50px'><div style='width:10px;max-width:15px;flex-grow:1;height:4px;background:red'></div><div style='width:10px;flex-grow:1;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            grow.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 15.0), (15.0, 35.0)]
        );
        let shrink = colored_boxes(
            "<div style='display:flex;width:40px'><div style='width:30px;min-width:25px;height:4px;background:red'></div><div style='width:30px;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            shrink.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 25.0), (25.0, 15.0)]
        );
    }

    #[test]
    fn flex_automatic_minimum_holds_min_content_but_zero_or_scroll_overflow_can_shrink() {
        let automatic = colored_boxes_in(
            "<main style='display:flex;width:220px'><div style='background:red'><div style='width:130px;height:6px'></div></div><div style='flex:0 1 150px;height:6px;background:green'></div></main>",
            240,
            100,
        );
        assert_eq!(automatic.len(), 2);
        assert!((automatic[0].width - 130.0).abs() < 0.01);
        assert!((automatic[1].width - 90.0).abs() < 0.01);

        let explicit_zero = colored_boxes_in(
            "<main style='display:flex;width:220px'><div style='min-width:0;background:red'><div style='width:130px;height:6px'></div></div><div style='flex:0 1 150px;height:6px;background:green'></div></main>",
            240,
            100,
        );
        assert_eq!(explicit_zero.len(), 2);
        assert!(explicit_zero[0].width < 130.0);
        assert!((explicit_zero.iter().map(|rect| rect.width).sum::<f32>() - 220.0).abs() < 0.01);

        let scrollable = colored_boxes_in(
            "<main style='display:flex;width:220px'><div style='overflow:auto;background:red'><div style='width:130px;height:6px'></div></div><div style='flex:0 1 150px;height:6px;background:green'></div></main>",
            240,
            100,
        );
        assert_eq!(scrollable.len(), 2);
        assert!(scrollable[0].width < 130.0);
    }

    #[test]
    fn flex_basis_content_uses_intrinsic_main_size_instead_of_preferred_width() {
        let boxes = colored_boxes(
            "<main style='display:flex;width:240px'><div style='width:20px;height:20px;background:green'><div style='width:120px;height:8px'></div></div><div style='width:20px;height:20px;flex-basis:content;background:red'><div style='width:120px;height:8px'></div></div></main>",
        );
        assert_eq!(boxes.len(), 2);
        assert_eq!(
            boxes.iter().map(|rect| rect.width).collect::<Vec<_>>(),
            [20.0, 120.0]
        );
    }

    #[test]
    fn flex_intrinsic_basis_keywords_measure_min_max_and_fit_content() {
        let boxes = colored_boxes(
            "<main style='display:flex;width:14px'><div style='flex:0 0 min-content;width:50px;height:4px;background:red'>aa bbbb ccc</div></main><main style='display:flex;width:100px'><div style='flex:0 0 max-content;width:50px;height:4px;background:green'>aa bbbb ccc</div></main><main style='display:flex;width:14px'><div style='flex:0 0 fit-content;width:50px;height:4px;background:red'>aa bbbb ccc</div></main>",
        );
        assert_eq!(
            boxes.iter().map(|rect| rect.width).collect::<Vec<_>>(),
            [8.0, 22.0, 14.0]
        );
    }

    #[test]
    fn flex_row_baseline_fallback_aligns_items_at_their_bottom_margin_edge() {
        let boxes = colored_boxes(
            "<main style='display:flex;align-items:baseline;width:100px;height:80px'><div style='width:40px;height:30px;background:red'></div><div style='width:20px;height:50px;background:green'></div><div style='width:20px;height:20px;background:yellow'></div></main>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 20.0, 30.0), (40.0, 0.0, 50.0), (60.0, 30.0, 20.0)]
        );
    }

    #[test]
    fn flex_column_baseline_falls_back_to_cross_start_for_orthogonal_items() {
        let boxes = colored_boxes(
            "<main style='display:flex;flex-direction:column;align-items:baseline;width:200px;height:100px'><div style='width:40px;height:30px;background:red'></div><div style='width:120px;height:20px;background:green'></div></main>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 40.0), (0.0, 30.0, 120.0)]
        );
    }

    #[test]
    fn flex_wrap_forms_lines_before_grow_and_aligns_each_line() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:24px;gap:2px;align-items:center'><div style='width:10px;height:4px;flex-grow:1;background:red'></div><div style='width:10px;height:8px;flex-grow:1;background:green'></div><div style='width:10px;height:6px;flex-grow:1;background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 2.0, 11.0, 4.0),
                (13.0, 0.0, 11.0, 8.0),
                (0.0, 10.0, 24.0, 6.0)
            ]
        );
    }

    #[test]
    fn flex_wrap_stretches_lines_in_definite_cross_size() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:20px;height:30px;gap:2px'><div style='width:12px;height:4px;background:red'></div><div style='width:12px;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 12.0, 4.0), (0.0, 18.0, 12.0, 12.0)]
        );
    }

    #[test]
    fn flex_column_wrap_and_oversized_item() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-direction:column;flex-wrap:wrap;width:30px;height:10px;gap:2px;align-items:start'><div style='width:4px;height:6px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 4.0, 6.0), (14.0, 0.0, 8.0, 6.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:10px'><div style='width:20px;height:4px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.width)).collect::<Vec<_>>(),
            [(0.0, 10.0), (4.0, 8.0)]
        );
    }

    #[test]
    fn flex_row_gap_justify_and_cross_alignment() {
        let boxes = colored_boxes(
            "<div style='display:flex;width:40px;height:20px;gap:4px;justify-content:space-between;align-items:center'><div style='width:8px;height:6px;background:red'></div><div style='width:8px;height:10px;background:green'></div></div>",
        );
        assert_eq!(
            boxes,
            [
                Rect {
                    x: 0.0,
                    y: 7.0,
                    width: 8.0,
                    height: 6.0
                },
                Rect {
                    x: 32.0,
                    y: 5.0,
                    width: 8.0,
                    height: 10.0
                }
            ]
        );
    }

    #[test]
    fn flex_grow_and_weighted_shrink_distribute_main_size() {
        let grow = colored_boxes(
            "<div style='display:flex;width:40px'><div style='width:10px;height:4px;flex-grow:1;background:red'></div><div style='width:10px;height:4px;flex-grow:3;background:green'></div></div>",
        );
        assert_eq!(
            grow.iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 15.0), (15.0, 25.0)]
        );
        let shrink = colored_boxes(
            "<div style='display:flex;width:10px'><div style='width:8px;height:4px;background:red'></div><div style='width:8px;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            shrink
                .iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 5.0), (5.0, 5.0)]
        );
    }

    #[test]
    fn flex_column_reverse_and_auto_height_stretch() {
        let column = colored_boxes(
            "<div style='display:flex;flex-direction:column-reverse;width:20px;height:20px;gap:2px;align-items:center'><div style='width:4px;height:4px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            column,
            [
                Rect {
                    x: 8.0,
                    y: 16.0,
                    width: 4.0,
                    height: 4.0
                },
                Rect {
                    x: 6.0,
                    y: 8.0,
                    width: 8.0,
                    height: 6.0
                }
            ]
        );
        let stretched = colored_boxes(
            "<div style='display:flex;width:20px'><div style='width:5px;height:4px;background:red'></div><div style='width:5px;background:green'></div></div>",
        );
        assert_eq!(
            stretched.iter().map(|rect| rect.height).collect::<Vec<_>>(),
            [4.0, 4.0]
        );
    }

    #[test]
    fn border_box_sizes_include_padding_border_and_flex_allocations() {
        let boxes = colored_boxes(
            "<div style='box-sizing:border-box;width:8px;height:8px;padding:2px;border:1px solid blue;background:red'></div>",
        );
        assert_eq!(
            boxes,
            [Rect {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 8.0
            }]
        );
        let flex = colored_boxes(
            "<div style='display:flex;width:40px'><div style='box-sizing:border-box;width:20px;height:8px;padding:2px;border:1px solid blue;background:red'></div><div style='box-sizing:border-box;width:20px;height:8px;padding:2px;border:1px solid blue;background:green'></div></div>",
        );
        assert_eq!(
            flex.iter()
                .map(|rect| (rect.x, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 20.0, 8.0), (20.0, 20.0, 8.0)]
        );
    }

    #[test]
    fn normal_whitespace_collapses_but_nbsp_does_not_break() {
        let plain = crate::html::parse("<p>a b</p>", 16).unwrap();
        let spaced = crate::html::parse("<p> \t a\n\r  b</p>", 16).unwrap();
        assert_eq!(
            display_list(&plain, 100, 30, &FixedText).unwrap(),
            display_list(&spaced, 100, 30, &FixedText).unwrap()
        );
        let nbsp = crate::html::parse("<p style='width:6px'>a&nbsp;b</p>", 16).unwrap();
        let list = display_list(&nbsp, 100, 30, &FixedText).unwrap();
        assert_eq!(
            list.0
                .iter()
                .filter(|c| matches!(c, Command::GlyphRun { .. }))
                .count(),
            1
        );
        assert!(matches!(collapsed_text("unchanged"), Cow::Borrowed(_)));
    }

    #[test]
    fn explicit_break_starts_next_line_and_hidden_break_does_not() {
        for (markup, expected) in [
            (
                "<style>body{margin:0}</style><p>a<br>b</p>",
                &[(0.0, 2.0), (0.0, 5.0)][..],
            ),
            (
                "<style>body{margin:0}</style><p>a<br style='display:none'>b</p>",
                // A display:none break has no paragraph effect, so adjacent
                // same-style text is shaped as one run.
                &[(0.0, 2.0)][..],
            ),
        ] {
            let document = crate::html::parse(markup, 16).unwrap();
            let list = display_list(&document, 100, 30, &FixedText).unwrap();
            let runs: Vec<_> = list
                .0
                .iter()
                .filter_map(|command| match command {
                    Command::GlyphRun {
                        origin_x,
                        baseline_y,
                        ..
                    } => Some((*origin_x, *baseline_y)),
                    _ => None,
                })
                .collect();
            assert_eq!(runs.as_slice(), expected);
        }
    }

    #[test]
    fn unitless_line_height_inherits_and_centers_glyphs() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><div style='line-height:2'><p style='font-size:4px'>a<br>b</p></div>",
            16,
        )
        .unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let baselines: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y),
                _ => None,
            })
            .collect();
        assert_eq!(baselines, [4.5, 12.5]);
    }

    #[test]
    fn template_subtrees_are_inert_for_style_paint_and_inline_flow() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p>a<template><style>p{background:red}</style><b>hidden</b></template>b</p>",
            24,
        )
        .unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        // The inert template contributes no boundary; the two adjacent text
        // nodes retain one shaping run.
        assert_eq!(runs, [(0.0, 2.0)]);
        assert!(!list.0.iter().any(|command| matches!(command, Command::FillRect { color, .. } if color.r == 255 && color.g == 0)));
    }

    #[test]
    fn foreign_namespace_elements_are_inert() {
        let mut document = crate::html::parse("<body></body>", 16).unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let foreign = document
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: "div".into(),
                attributes: alloc::vec![(
                    "style".into(),
                    "background:red;width:10px;height:10px".into()
                )],
            })
            .unwrap();
        document.append(body, foreign).unwrap();
        let text = document.create(NodeKind::Text("inert".into())).unwrap();
        document.append(foreign, text).unwrap();
        let list = display_list(&document, 20, 20, &NoShape).unwrap();
        assert_eq!(list.0.len(), 1);
    }

    impl TextShaper for FixedText {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun {
                glyphs: Arc::from([]),
                width: text.len() as f32 * 2.0,
            })
        }
        fn ascent(&self, _: f32) -> f32 {
            2.0
        }
        fn line_height(&self, _: f32) -> f32 {
            3.0
        }
    }

    #[derive(Default)]
    struct RecordingText(core::cell::RefCell<Vec<String>>);

    impl TextShaper for RecordingText {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            self.0.borrow_mut().push(String::from(text));
            Ok(ShapedRun {
                glyphs: Arc::from([]),
                width: text.chars().count() as f32 * 6.0,
            })
        }

        fn ascent(&self, _: f32) -> f32 {
            10.0
        }

        fn line_height(&self, _: f32) -> f32 {
            16.0
        }
    }

    fn recorded_shapes(markup: &str) -> Vec<String> {
        let document = crate::html::parse(markup, 128).unwrap();
        let text = RecordingText::default();
        display_list(&document, 320, 200, &text).unwrap();
        text.0.into_inner()
    }

    #[test]
    fn generated_inline_pseudos_join_text_flow_and_keep_their_style() {
        let document = crate::html::parse(
            "<style>body,p{margin:0}#target::before{content:'pre';color:red;text-decoration:underline;font-size:20px}#target::after{content:attr(data-tail, 'fallback');color:blue}</style><p><span id='target' data-tail='post'>mid</span></p>",
            128,
        )
        .unwrap();
        let text = RecordingText::default();
        let list = display_list(&document, 120, 60, &text).unwrap();
        let shaped = text.0.into_inner();
        for expected in ["pre", "mid", "post"] {
            assert!(
                shaped.iter().any(|run| run == expected),
                "missing {expected:?} in {shaped:?}"
            );
        }
        assert!(list.0.iter().any(|command| matches!(
            command,
            Command::GlyphRun { size, color, .. }
                if *size == 20.0 && color.r == 255 && color.g == 0
        )));
        assert!(list.0.iter().any(|command| matches!(
            command,
            Command::FillRect { color, .. }
                if color.r == 255 && color.g == 0 && color.b == 0
        )));
    }

    #[test]
    fn generated_before_renders_for_an_empty_block_origin_and_attr_fallback() {
        let empty_origin = recorded_shapes(
            "<style>body{margin:0}#blank:empty::before{content:'empty-origin'}</style><div id='blank'></div>",
        );
        assert!(empty_origin.iter().any(|run| run == "empty-origin"));

        let fallback = recorded_shapes(
            "<style>body{margin:0}#target::before{content:attr(data-missing, 'fallback')}</style><div id='target'></div>",
        );
        assert!(fallback.iter().any(|run| run == "fallback"));
    }

    #[test]
    fn generated_attr_uses_html_case_rules_and_null_attribute_namespace() {
        let html = recorded_shapes(
            "<style>body{margin:0}#target::before{content:attr(DATA-LABEL, 'fallback')}</style><div id='target' data-label='html-value'></div>",
        );
        assert!(html.iter().any(|run| run == "html-value"));

        let xml = crate::xml::parse(
            "<html xmlns='http://www.w3.org/1999/xhtml' xmlns:other='urn:other'><head><style>body{margin:0}#target::before{content:attr(DATA-LABEL, 'fallback')}</style></head><body><div id='target' data-label='plain-value' other:data-label='namespaced-value'/></body></html>",
            128,
        )
        .unwrap();
        let text = RecordingText::default();
        display_list(&xml, 120, 60, &text).unwrap();
        let shaped = text.0.into_inner();
        assert!(shaped.iter().any(|run| run == "fallback"));
        assert!(
            !shaped
                .iter()
                .any(|run| run == "plain-value" || run == "namespaced-value")
        );
    }

    #[test]
    fn generated_block_boundaries_share_origin_hit_owner_without_element_rects() {
        let document = crate::html::parse(
            "<style>body{margin:0}#target::before{content:'before'}#target::after{content:'after'}</style><div id='target'>middle</div>",
            128,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let rules = stylesheets(&document).unwrap();
        let text = RecordingText::default();
        let mut geometry = LayoutGeometry::default();
        display_list_with_styles(
            &document,
            120,
            60,
            &text,
            &rules,
            None,
            Some(&mut geometry),
            &[],
        )
        .unwrap();
        let shaped = text.0.into_inner();
        for expected in ["before", "middle", "after"] {
            assert!(
                shaped.iter().any(|run| run == expected),
                "missing {expected:?}"
            );
        }
        assert!(
            geometry
                .hits
                .iter()
                .any(|hit| hit.node == target && hit.virtual_generated)
        );
        assert!(
            geometry
                .hits
                .iter()
                .any(|hit| hit.node == target && !hit.virtual_generated)
        );
    }

    #[test]
    fn generated_block_pseudos_paint_and_advance_as_virtual_block_boxes() {
        let document = crate::html::parse(
            "<style>body{margin:0}#target::before{content:'before';display:block;width:30px;height:10px;background:red}#target::after{content:'after';display:block;width:40px;height:12px;background:blue}</style><div id='target'>middle</div>",
            128,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let rules = stylesheets(&document).unwrap();
        let text = RecordingText::default();
        let mut geometry = LayoutGeometry::default();
        let list = display_list_with_styles(
            &document,
            120,
            80,
            &text,
            &rules,
            None,
            Some(&mut geometry),
            &[],
        )
        .unwrap();
        let shaped = text.0.into_inner();
        for expected in ["before", "middle", "after"] {
            assert!(
                shaped.iter().any(|run| run == expected),
                "missing {expected:?}: {shaped:?}"
            );
        }
        let red = list.0.iter().find_map(|command| match command {
            Command::FillRect { rect, color } if color.r == 255 && color.g == 0 && color.b == 0 => {
                Some(*rect)
            }
            _ => None,
        });
        let blue = list.0.iter().find_map(|command| match command {
            Command::FillRect { rect, color } if color.r == 0 && color.g == 0 && color.b == 255 => {
                Some(*rect)
            }
            _ => None,
        });
        assert_eq!(red.map(|rect| (rect.y, rect.height)), Some((0.0, 10.0)));
        assert!(blue.is_some_and(|rect| rect.y >= 10.0 && rect.height == 12.0));
        assert!(
            geometry
                .hits
                .iter()
                .any(|hit| hit.node == target && hit.virtual_generated)
        );
    }

    #[test]
    fn generated_pseudos_participate_in_flex_and_grid_item_layout() {
        let document = crate::html::parse(
            "<style>body{margin:0}#flex{display:flex;width:160px}#flex::before{content:'flex-before'}#flex::after{content:'flex-after'}#grid{display:grid;width:80px;grid-template-columns:40px 40px}#grid::before{content:'grid-before';grid-column:1}#grid::after{content:'grid-after';grid-column:2}</style><div id='flex'></div><div id='grid'></div>",
            128,
        )
        .unwrap();
        let text = RecordingText::default();
        let list = display_list(&document, 200, 120, &text).unwrap();
        let shaped = text.0.into_inner();
        for expected in ["flex-before", "flex-after", "grid-before", "grid-after"] {
            assert!(
                shaped.iter().any(|run| run == expected),
                "missing {expected:?}: {shaped:?}"
            );
        }
        let origins: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { origin_x, .. } => Some(*origin_x),
                _ => None,
            })
            .collect();
        assert!(
            origins.iter().any(|x| (*x - 40.0).abs() < 0.01),
            "grid item was not placed in column 2: {origins:?}"
        );
        assert!(
            origins.iter().any(|x| (*x - 66.0).abs() < 0.01),
            "flex after-item did not follow the before-item: {origins:?}"
        );
        assert!(!list.0.is_empty());
    }

    #[test]
    fn unsupported_generated_content_and_item_layout_fail_explicitly() {
        for markup in [
            "<style>div::before{content:counter(chapter)}</style><div></div>",
            "<style>div::before{content:open-quote}</style><div></div>",
            "<style>div::before{content:url(icon.svg)}</style><div></div>",
            "<style>div::before{content:'x';display:inline-block}</style><div></div>",
        ] {
            let document = crate::html::parse(markup, 128).unwrap();
            assert!(
                matches!(
                    display_list(&document, 120, 60, &FixedText),
                    Err(LayoutError::UnsupportedGeneratedContent)
                ),
                "generated feature was not reported for {markup}"
            );
        }
    }

    #[test]
    fn form_controls_paint_live_values_placeholders_and_textarea_content() {
        let shaped = recorded_shapes(
            "<style>body{margin:0}input,textarea{width:160px;height:40px;padding:0;border:0}</style><input value='live value'><input placeholder='hint text'><textarea>default text</textarea><textarea value='live textarea'>stale default</textarea><textarea placeholder='draft text'></textarea>",
        );
        for expected in [
            "live value",
            "hint text",
            "default text",
            "live textarea",
            "draft text",
        ] {
            assert!(
                shaped.iter().any(|text| text == expected),
                "missing {expected:?}: {shaped:?}"
            );
        }
        assert!(!shaped.iter().any(|text| text == "stale default"));
    }

    #[test]
    fn password_is_masked_by_grapheme_and_nontext_control_values_stay_private() {
        let shaped = recorded_shapes(
            "<style>body{margin:0}</style><input type='password' value='á👩‍❤️‍💋‍👨'><input type='checkbox' checked value='must-not-leak'><input type='hidden' value='also-private'>",
        );
        assert_eq!(shaped, ["••"]);
    }

    #[test]
    fn password_geometry_keeps_source_grapheme_ranges_and_composed_transform() {
        let source = "a\u{301}👩‍❤️‍💋‍👨";
        let markup = alloc::format!(
            "<style>body{{margin:0}}input{{width:80px;height:20px}}</style><input type='password' value='{source}' style='transform:translate(9px,0)'>"
        );
        let document = crate::html::parse(&markup, 32).unwrap();
        let mut rules = stylesheets(&document).unwrap();
        rules.environment = css::MediaEnvironment {
            width: 100.0,
            height: 60.0,
            ..Default::default()
        };
        let mut geometry = LayoutGeometry::default();
        display_list_with_styles(
            &document,
            100,
            60,
            &FixedText,
            &rules,
            None,
            Some(&mut geometry),
            &[],
        )
        .unwrap();
        assert_eq!(geometry.control_text_runs.len(), 1);
        let run = &geometry.control_text_runs[0];
        assert_eq!(run.range, 0..source.len());
        assert_eq!(run.display_range, 0..6);
        assert!(run.password);
        let transformed = run.transform.apply(run.x, run.y);
        assert!((transformed.0 - (run.x + 9.0)).abs() < 0.001);
        assert!((transformed.1 - run.y).abs() < 0.001);
    }

    #[test]
    fn control_text_over_budget_fails_without_rendering_a_truncated_value() {
        let value = "x".repeat(MAX_CONTROL_TEXT_BYTES + 1);
        let markup = alloc::format!("<style>body{{margin:0}}</style><input value='{value}'>");
        let document = crate::html::parse(&markup, 32).unwrap();
        assert_eq!(
            display_list(&document, 100, 40, &FixedText),
            Err(LayoutError::CommandLimit)
        );
    }

    #[test]
    fn authored_control_styles_override_ua_defaults() {
        let document = crate::html::parse(
            "<style>input{display:block;width:77px;height:9px;font-size:18px;font-family:serif;padding:0;border:0;background:transparent}</style><input id='field' size='4'>",
            128,
        )
        .unwrap();
        let index = stylesheets(&document).unwrap();
        let mut pending = alloc::vec![document.root()];
        let mut input = None;
        while let Some(node) = pending.pop() {
            if let NodeKind::Element {
                name, attributes, ..
            } = document.kind(node).expect("valid node")
            {
                if name == "input"
                    && attributes
                        .iter()
                        .any(|(key, value)| key == "id" && value == "field")
                {
                    input = Some(node);
                    break;
                }
            }
            let mut child = document.first_child(node).unwrap();
            while let Some(current) = child {
                pending.push(current);
                child = document.next_sibling(current).unwrap();
            }
        }
        let style = css::compute_node(&document, input.unwrap(), None, &index).unwrap();
        assert_eq!(style.display, Display::Block);
        assert_eq!(style.width, Some(77.0));
        assert_eq!(style.height, Some(9.0));
        assert_eq!(style.font_size, 18.0);
        assert_eq!(style.padding_sides, [0.0; 4]);
        assert!(!style.border_solid);
        assert_eq!(style.background.a, 0);
        assert_eq!(style.font.families.as_deref().unwrap()[0].as_ref(), "serif");
    }

    struct DirectionText(core::cell::RefCell<Vec<(String, bool)>>);

    impl TextShaper for DirectionText {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun {
                glyphs: Arc::from([]),
                width: text.len() as f32 * 2.0,
            })
        }
        fn shape_directional(&self, text: &str, _: f32, rtl: bool) -> Result<ShapedRun, ()> {
            self.0.borrow_mut().push((String::from(text), rtl));
            self.shape(text, 16.0)
        }
        fn ascent(&self, _: f32) -> f32 {
            2.0
        }
        fn line_height(&self, _: f32) -> f32 {
            3.0
        }
    }

    #[test]
    fn inline_text_and_elements_share_a_line() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p>Hello <strong>world</strong>!</p><div>next</div>",
            16,
        )
        .unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [(0.0, 2.0), (12.0, 2.0), (22.0, 2.0), (0.0, 5.0)]);
    }

    #[test]
    fn styled_inline_spans_share_paragraph_bidi_resolution() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p style='margin:0'><span style='color:red'>abc </span><strong style='color:blue'>אבג</strong><span style='color:green'>123</span></p>",
            16,
        )
        .unwrap();
        let text = DirectionText(core::cell::RefCell::new(Vec::new()));
        let list = display_list(&document, 100, 30, &text).unwrap();
        let runs = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x, color, ..
                } => Some((*origin_x, *color)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            runs.iter()
                .map(|(x, color)| (*x, (color.r, color.g, color.b)))
                .collect::<Vec<_>>(),
            [(0.0, (255, 0, 0)), (8.0, (0, 128, 0)), (14.0, (0, 0, 255))]
        );
        let shaped = text.0.borrow();
        assert!(shaped.iter().any(|(value, rtl)| value == "אבג" && *rtl));
        assert!(shaped.iter().any(|(value, rtl)| value == "123" && !*rtl));
        assert!(shaped.iter().any(|(value, rtl)| value == "abc " && !*rtl));
    }

    #[test]
    fn adjacent_text_nodes_with_one_style_shape_as_a_single_run() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p style='margin:0'>of<!--split-->fice</p>",
            16,
        )
        .unwrap();
        let text = DirectionText(core::cell::RefCell::new(Vec::new()));
        display_list(&document, 100, 30, &text).unwrap();
        assert!(
            text.0
                .borrow()
                .iter()
                .any(|(value, rtl)| value == "office" && !*rtl),
            "adjacent text nodes in one style run stay together for shaping"
        );
    }

    #[test]
    fn inline_backgrounds_paint_one_fragment_per_wrapped_line() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p style='margin:0;width:8px;line-height:3px'><span style='background:#ff0000'>ab cd</span></p>",
            16,
        )
        .unwrap();
        let fills = display_list(&document, 40, 30, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.r == 255 && color.g == 0 => Some(rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fills
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 6.0, 3.0), (0.0, 3.0, 4.0, 3.0)]
        );
    }

    #[test]
    fn inline_atoms_keep_their_paragraph_position_and_margin_box_width() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p style='margin:0'>A<span style='display:inline-block;width:4px;height:4px;margin-left:2px;margin-right:1px;background:red'></span>B</p>",
            16,
        )
        .unwrap();
        let list = display_list(&document, 40, 30, &FixedText).unwrap();
        let runs = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { origin_x, .. } => Some(*origin_x),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(runs, [0.0, 9.0]);
    }

    #[test]
    fn text_and_inline_elements_wrap_at_box_width() {
        let document = crate::html::parse(
            "<style>body{margin:0}</style><p style='width:10px'>aaa <strong>bbbb</strong> cccc</p>",
            12,
        )
        .unwrap();
        let list = display_list(&document, 40, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [(0.0, 2.0), (0.0, 5.0), (8.0, 5.0), (0.0, 8.0)]);
    }

    #[test]
    fn rounded_background_bleed_inset_requires_opaque_continuous_border() {
        for (border, inset) in [
            ("2px solid #0080ff", 1.0),
            ("2px solid rgba(0,128,255,0.5)", 0.0),
            ("2px dashed #0080ff", 0.0),
            ("2px dotted #0080ff", 0.0),
        ] {
            let html = alloc::format!(
                "<style>body{{margin:0}}</style><div style='width:20px;border:{border};border-radius:6px;background:#ff8000'><div style='height:16px'></div></div>"
            );
            let document = crate::html::parse(&html, 16).unwrap();
            let list = display_list(&document, 40, 40, &FixedText).unwrap();
            let (rect, radius) = list
                .0
                .iter()
                .find_map(|command| match command {
                    Command::FillRoundedRect {
                        rect,
                        radius,
                        color,
                        ..
                    } if color.r == 255 && color.g == 128 => Some((*rect, *radius)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                rect,
                Rect {
                    x: inset,
                    y: inset,
                    width: 24.0 - 2.0 * inset,
                    height: 20.0 - 2.0 * inset
                },
                "{border}"
            );
            assert_eq!(radius, 6.0 - inset, "{border}");
            assert!(
                !list
                    .0
                    .iter()
                    .any(|command| matches!(command, Command::PushLayer { .. }))
            );
        }
    }

    #[test]
    fn grid_repeat_minmax_percent_implicit_flow_and_alignment() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:repeat(2,minmax(0,1fr));grid-auto-rows:10px;gap:4px'><div style='background:red'></div><div style='background:green'></div><div style='background:#ff8000'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 0.0, 48.0, 10.0),
                (52.0, 0.0, 48.0, 10.0),
                (0.0, 14.0, 48.0, 10.0)
            ]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:25% 1fr;grid-template-rows:10px 10px;grid-auto-flow:column'><div style='background:red'></div><div style='background:green'></div><div style='background:#ff8000'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 25.0), (0.0, 10.0, 25.0), (25.0, 0.0, 75.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;height:40px;grid-template-columns:20px 20px;grid-template-rows:10px;justify-content:center;align-content:end;justify-items:center;align-items:center'><div style='width:10px;height:4px;background:red'></div></div>",
        );
        assert_eq!(
            (boxes[0].x, boxes[0].y, boxes[0].width, boxes[0].height),
            (35.0, 33.0, 10.0, 4.0)
        );
    }

    #[test]
    fn grid_named_lines_areas_rtl_and_subgrid_inherit_tracks() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:[a] 30px [b] 70px [c];grid-template-rows:10px'><div style='grid-column:b / c;background:red'></div></div>",
        );
        assert_eq!((boxes[0].x, boxes[0].width), (30.0, 70.0));
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px 20px;grid-template-areas:\"top top\" \"left right\"'><div style='grid-area:right;background:red'></div></div>",
        );
        assert_eq!(
            (boxes[0].x, boxes[0].y, boxes[0].width, boxes[0].height),
            (30.0, 10.0, 70.0, 20.0)
        );
        let boxes = colored_boxes(
            "<div style='display:grid;direction:rtl;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px'><div style='background:red'></div><div style='background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(70.0, 30.0), (0.0, 70.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px'><div style='display:grid;grid-column:1 / 3;grid-template-columns:subgrid;grid-template-rows:subgrid'><div style='background:red'></div><div style='background:green'></div></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 30.0, 10.0), (30.0, 70.0, 10.0)]
        );
    }
}
