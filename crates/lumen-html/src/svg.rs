//! Bounded, renderer-neutral helpers for the first SVG viewport and geometry
//! subset. Path-data parsing remains in the shared image backend so Canvas and
//! inline SVG use the same validated parser.
use alloc::{string::String, sync::Arc, vec::Vec};

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
    if values.len() != 4 || values[2] < 0.0 || values[3] < 0.0 {
        return None;
    }
    Some(ViewBox {
        min_x: values[0],
        min_y: values[1],
        width: values[2],
        height: values[3],
    })
}

/// The view overrides are parsed once per URL fragment, then resolved against
/// the actual document's existing ID index. No native or node owner is retained.
#[derive(Clone, Debug, PartialEq)]
pub enum Fragment {
    Named(Arc<str>),
    View(ViewSpec),
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewSpec {
    pub view_box: Option<ViewBox>,
    pub aspect: Option<AspectRatio>,
    pub transform: Option<Affine>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AspectRatio {
    x: u8,
    y: u8,
    mode: u8,
}
impl Default for AspectRatio {
    fn default() -> Self {
        Self {
            x: 1,
            y: 1,
            mode: 1,
        }
    }
}
pub fn parse_aspect_ratio(input: &str) -> Option<AspectRatio> {
    let mut parts = input.split_ascii_whitespace();
    let align = parts.next()?;
    let mode = match parts.next() {
        None | Some("meet") => 1,
        Some("slice") => 2,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    if align == "none" {
        return Some(AspectRatio {
            x: 0,
            y: 0,
            mode: 0,
        });
    }
    let (x, y) = match align {
        "xMinYMin" => (0, 0),
        "xMidYMin" => (1, 0),
        "xMaxYMin" => (2, 0),
        "xMinYMid" => (0, 1),
        "xMidYMid" => (1, 1),
        "xMaxYMid" => (2, 1),
        "xMinYMax" => (0, 2),
        "xMidYMax" => (1, 2),
        "xMaxYMax" => (2, 2),
        _ => return None,
    };
    Some(AspectRatio { x, y, mode })
}
impl Fragment {
    /// SVG2 view parameters are atomic, ordered arbitrarily, and each occurs
    /// at most once. URL escaping is decoded by the shared caller beforehand.
    pub fn parse(decoded: &str) -> Option<Self> {
        if decoded.is_empty() || decoded.len() > MAX_TRANSFORM_BYTES {
            return None;
        }
        if !decoded.starts_with("svgView(") {
            return crate::xml::is_xml_name(decoded).then(|| Self::Named(Arc::from(decoded)));
        }
        let body = decoded.strip_prefix("svgView(")?.strip_suffix(')')?;
        let mut rest = body.trim_matches([' ', '\t', '\r', '\n']);
        let mut result = ViewSpec::default();
        let mut count = 0;
        loop {
            count += 1;
            if count > 3 {
                return None;
            }
            let open = rest.find('(')?;
            let name = rest[..open].trim_matches([' ', '\t', '\r', '\n']);
            let mut depth = 1usize;
            let mut close = None;
            for (offset, byte) in rest.bytes().enumerate().skip(open + 1) {
                match byte {
                    b'(' => {
                        depth = depth.checked_add(1)?;
                        if depth > 3 {
                            return None;
                        }
                    }
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let close = close?;
            let value = &rest[open + 1..close];
            match name {
                "viewBox" if result.view_box.is_none() => {
                    result.view_box = Some(parse_view_box(value)?)
                }
                "preserveAspectRatio" if result.aspect.is_none() => {
                    result.aspect = Some(parse_aspect_ratio(value)?)
                }
                "transform" if result.transform.is_none() => {
                    result.transform = Some(parse_transform(value)?)
                }
                _ => return None,
            }
            rest = rest[close + 1..].trim_matches([' ', '\t', '\r', '\n']);
            if rest.is_empty() {
                return Some(Self::View(result));
            }
            rest = rest
                .strip_prefix(';')?
                .trim_matches([' ', '\t', '\r', '\n']);
            if rest.is_empty() {
                return None;
            }
        }
    }
    pub fn resolve(&self, document: &crate::Document) -> Option<ViewSpec> {
        match self {
            Self::View(view) => Some(*view),
            Self::Named(id) => {
                let node =
                    crate::selector::get_element_by_id(document, document.root(), id).ok()??;
                let crate::NodeKind::Element {
                    namespace: crate::Namespace::Svg,
                    name,
                    attributes,
                } = document.kind(node).ok()?
                else {
                    return None;
                };
                if local_name(name) != "view" {
                    return None;
                }
                Some(ViewSpec {
                    view_box: attribute(attributes, "viewBox").and_then(parse_view_box),
                    aspect: attribute(attributes, "preserveAspectRatio")
                        .and_then(parse_aspect_ratio),
                    transform: None,
                })
            }
        }
    }
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
    root_size_with_view(
        attributes,
        css_width,
        css_height,
        available_width,
        available_height,
        None,
    )
}

pub fn root_size_with_view(
    attributes: &[(Name, String)],
    css_width: Option<f32>,
    css_height: Option<f32>,
    available_width: f32,
    available_height: Option<f32>,
    view: Option<ViewSpec>,
) -> (f32, f32) {
    let view_box = view
        .and_then(|view| view.view_box)
        .or_else(|| attribute(attributes, "viewBox").and_then(parse_view_box));
    let width = css_width.or_else(|| {
        attribute(attributes, "width").and_then(|value| dimension(value, available_width))
    });
    let height = css_height.or_else(|| {
        attribute(attributes, "height")
            .and_then(|value| dimension(value, available_height.unwrap_or(150.0)))
    });
    let ratio = view_box
        .map(|view_box| view_box.width / view_box.height)
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0);
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

fn dimension(input:&str,percentage_basis:f32)->Option<f32> {
    coordinate_value(input,percentage_basis).filter(|value|*value>=0.0)
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
        coordinate_length(value).or_else(||crate::css::resolved_svg_coordinate_expression(value,percentage_basis))?
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

/// SVG2 §6.6 exhaustively defines presentation attributes. New CSS properties
/// do not automatically gain an attribute, and geometry name clashes are local
/// to the designated elements. Values still use the canonical CSS parser.
pub(crate) fn presentation_property<'a>(tag:&str,name:&'a str)->Option<&'a str> {
    let property=match name {
        "gradientTransform" if matches!(tag,"linearGradient"|"radialGradient")=>"transform",
        "patternTransform" if tag=="pattern"=>"transform",
        "transform" if !matches!(tag,"pattern"|"linearGradient"|"radialGradient")=>"transform",
        "cx"|"cy" if matches!(tag,"circle"|"ellipse")=>name,
        "x"|"y"|"width"|"height" if matches!(tag,"foreignObject"|"image"|"rect"|"svg"|"symbol"|"use")=>name,
        "r" if tag=="circle"=>name,
        "rx"|"ry" if matches!(tag,"ellipse"|"rect")=>name,
        "d" if tag=="path"=>name,
        "fill" if !matches!(tag,"animate"|"animateMotion"|"animateTransform"|"set")=>name,
        "alignment-baseline"|"baseline-shift"|"clip-path"|"clip-rule"|"color"|
        "color-interpolation"|"color-interpolation-filters"|"cursor"|"direction"|"display"|
        "dominant-baseline"|"fill-opacity"|"fill-rule"|"filter"|"flood-color"|"flood-opacity"|
        "font-family"|"font-size"|"font-size-adjust"|"font-stretch"|"font-style"|"font-variant"|
        "font-weight"|"glyph-orientation-vertical"|"image-rendering"|"letter-spacing"|
        "lighting-color"|"marker-end"|"marker-mid"|"marker-start"|"mask"|"mask-type"|
        "opacity"|"overflow"|"paint-order"|"pointer-events"|"shape-rendering"|"stop-color"|
        "stop-opacity"|"stroke"|"stroke-dasharray"|"stroke-dashoffset"|"stroke-linecap"|
        "stroke-linejoin"|"stroke-miterlimit"|"stroke-opacity"|"stroke-width"|"text-anchor"|
        "text-decoration"|"text-overflow"|"text-rendering"|"transform-origin"|"unicode-bidi"|
        "vector-effect"|"visibility"|"white-space"|"word-spacing"|"writing-mode"=>name,
        _=>return None,
    };
    Some(property)
}

/// SVG transform aliases participate in the same author presentation cascade.
/// Gradient/pattern templates supply a missing attribute, never a second CSS cascade.
pub(crate) fn presentation_transform(tag:&str,attributes:&[(Name,String)],
    context:Option<(&crate::Document,crate::NodeId)>)->Option<Affine> {
    let key=match tag {"linearGradient"|"radialGradient"=>"gradientTransform","pattern"=>"patternTransform",_=>"transform"};
    if let Some(value)=attribute(attributes,key){return parse_transform(value);}
    if key=="transform" {return None;}
    let (document,mut node)=context?;
    let root=document.root_node(node,false).ok()?;
    for _ in 0..32 {
        let crate::NodeKind::Element{attributes,..}=document.kind(node).ok()? else{return None;};
        let href=attribute(attributes,"href")?.strip_prefix('#')?;
        let next=crate::selector::get_element_by_id(document,root,href).ok()??;
        // The fixed source chain bounds both memory and template cycles.
        if next==node {return None;}
        node=next;
        let crate::NodeKind::Element{namespace:crate::Namespace::Svg,name,attributes}=document.kind(node).ok()? else{return None;};
        if (key=="gradientTransform" && !matches!(local_name(name),"linearGradient"|"radialGradient"))
            || (key=="patternTransform" && local_name(name)!="pattern") {return None;}
        if let Some(value)=attribute(attributes,key){return parse_transform(value);}
    }
    None
}

/// Nested viewport geometry uses the same coordinate and viewBox authorities
/// as primitive geometry and the root viewport. Missing dimensions are 100%.
pub fn nested_viewport_with_geometry(attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,parent:ViewBox)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    nested_viewport_geometry_mode(attributes,geometry,css_width,css_height,parent,false,None)
}
fn nested_viewport_geometry_mode(attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,parent:ViewBox,computed:bool,context:Option<&crate::css::SvgCoordinateContext<'_>>)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    let coordinate=|index:usize,key:&str,basis:f32,default:f32| {
        geometry[index].as_deref().or_else(||(!computed || index==2 || index==3).then(||attribute(attributes,key)).flatten())
            .map(|value|context.map_or_else(||coordinate_value(value,basis),|context|if matches!(index,2|3){context.dimension(value,basis)}else{context.coordinate(value,basis)})).unwrap_or(Some(default))
    };
    let rect=crate::paint::Rect{x:coordinate(0,"x",parent.width,0.0)?,y:coordinate(1,"y",parent.height,0.0)?,
        width:css_width.or_else(||coordinate(2,"width",parent.width,parent.width))?,
        height:css_height.or_else(||coordinate(3,"height",parent.height,parent.height))?};
    if !rect.is_valid(){return None;}
    let view_box=attribute(attributes,"viewBox").and_then(parse_view_box);
    let (projection,user)=view_box_transform(view_box,attributes,rect.x,rect.y,rect.width,rect.height);
    Some((rect,projection,user))
}

/// An instantiated symbol uses nested SVG viewport geometry, with an
/// optional reference point in its contents' post-viewBox coordinate system.
/// Missing reference coordinates deliberately differ from explicit zero.
#[cfg(test)]
pub(crate) fn instance_viewport_with_geometry(tag:&str,attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    width:Option<f32>,height:Option<f32>,parent:ViewBox)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    instance_viewport_geometry_mode(tag,attributes,geometry,width,height,parent,false,None)
}
#[cfg(test)]
pub(crate) fn instance_viewport_with_computed_geometry(tag:&str,attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    width:Option<f32>,height:Option<f32>,parent:ViewBox)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    instance_viewport_geometry_mode(tag,attributes,geometry,width,height,parent,true,None)
}
pub(crate) fn instance_viewport_with_context(tag:&str,attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    width:Option<f32>,height:Option<f32>,parent:ViewBox,context:&crate::css::SvgCoordinateContext<'_>)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    instance_viewport_geometry_mode(tag,attributes,geometry,width,height,parent,true,Some(context))
}
fn instance_viewport_geometry_mode(tag:&str,attributes:&[(Name,String)],geometry:&[Option<Arc<str>>;9],
    width:Option<f32>,height:Option<f32>,parent:ViewBox,computed:bool,context:Option<&crate::css::SvgCoordinateContext<'_>>)->Option<(crate::paint::Rect,Affine,ViewBox)> {
    let (mut rect,mut projection,user)=nested_viewport_geometry_mode(attributes,geometry,width,height,parent,computed,context)?;
    if tag=="symbol" {
        for horizontal in [true,false] {
            let key=if horizontal{"refX"}else{"refY"};
            let Some(raw)=attribute(attributes,key) else{continue;};
            let raw=symbol_reference_keyword(raw,horizontal).unwrap_or(raw);
            let basis=if horizontal{user.width}else{user.height};
            let Some(value)=context.map_or_else(||coordinate_value(raw,basis),|context|context.coordinate(raw,basis)) else{continue;};
            if horizontal {
                let delta=rect.x-(projection.a*value+projection.e);
                rect.x+=delta;projection.e+=delta;
            }else{
                let delta=rect.y-(projection.d*value+projection.f);
                rect.y+=delta;projection.f+=delta;
            }
        }
    }
    (rect.is_valid()&&projection.is_finite()).then_some((rect,projection,user))
}

fn symbol_reference_keyword(raw:&str,horizontal:bool)->Option<&'static str> {
    match (raw.trim(),horizontal) {
        ("left",true)|("top",false)=>Some("0%"),
        ("center",_)=>Some("50%"),
        ("right",true)|("bottom",false)=>Some("100%"),
        _=>None,
    }
}
pub(crate) fn supports_symbol_reference(raw:&str,horizontal:bool)->bool {
    symbol_reference_keyword(raw,horizontal).is_some()||crate::css::svg_xml_coordinate_supported(raw)
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
    if width <= 0.0 || height <= 0.0 || view_box.width <= 0.0 || view_box.height <= 0.0 {
        return (Affine::IDENTITY, view_box);
    }
    let aspect = attribute(attributes, "preserveAspectRatio")
        .and_then(parse_aspect_ratio)
        .unwrap_or_default();
    view_box_transform_with_aspect(Some(view_box), aspect, x, y, width, height)
}

pub fn view_box_transform_with_aspect(
    view_box: Option<ViewBox>,
    aspect: AspectRatio,
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
    if width <= 0.0 || height <= 0.0 || view_box.width <= 0.0 || view_box.height <= 0.0 {
        return (Affine::IDENTITY, view_box);
    }
    let sx = width / view_box.width;
    let sy = height / view_box.height;
    let (scale_x, scale_y) = if aspect.mode == 0 {
        (sx, sy)
    } else {
        let scale = if aspect.mode == 2 {
            sx.max(sy)
        } else {
            sx.min(sy)
        };
        (scale, scale)
    };
    let spare_x = width - view_box.width * scale_x;
    let spare_y = height - view_box.height * scale_y;
    let align_x = f32::from(aspect.x) * 0.5;
    let align_y = f32::from(aspect.y) * 0.5;
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
        let mut values=[0.0f32;6];let mut length=0;let mut rotation_degrees=None;
        number_list_tokens_each(&rest[open+1..close],6,|token| {
            let Some(value)=token.parse::<f32>().ok().filter(|value|value.is_finite()) else{return false;};
            if name=="rotate"&&length==0 {rotation_degrees=token.parse::<f64>().ok().filter(|value|value.is_finite());}
            values[length]=value;length+=1;true
        })?;
        let args=&values[..length];
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
                let (sin,cos)=lumen_common::dom_geometry::sin_cos_degrees(rotation_degrees?);
                let (sin,cos)=(sin as f32,cos as f32);
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
    shape_path_geometry_mode(tag,attributes,user_box,geometry,css_width,css_height,false,false,None)
}

/// Bounding geometry preserves zero-sized shapes, which SVG2 requires even
/// when their rendering path is empty. Coordinates use the same authority.
pub fn shape_bounding_path_with_geometry(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,
)->Option<String>{shape_path_geometry_mode(tag,attributes,user_box,geometry,css_width,css_height,true,false,None)}

#[cfg(test)]
pub(crate) fn shape_path_with_computed_geometry(tag:&str,attributes:&[(Name,String)],user_box:ViewBox,
    geometry:&[Option<Arc<str>>;9],css_width:Option<f32>,css_height:Option<f32>)->Option<String> {
    shape_path_geometry_mode(tag,attributes,user_box,geometry,css_width,css_height,false,true,None)
}

pub(crate) fn shape_path_with_context(tag:&str,attributes:&[(Name,String)],user_box:ViewBox,
    geometry:&[Option<Arc<str>>;9],css_width:Option<f32>,css_height:Option<f32>,context:&crate::css::SvgCoordinateContext<'_>)->Option<String> {
    shape_path_geometry_mode(tag,attributes,user_box,geometry,css_width,css_height,false,true,Some(context))
}

fn shape_path_geometry_mode(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,bounding:bool,computed:bool,context:Option<&crate::css::SvgCoordinateContext<'_>>,
)->Option<String>{
    let mut output=String::new();let mut failed=false;
    write_shape_geometry(tag,attributes,user_box,geometry,css_width,css_height,bounding,computed,context,&mut output,None,&mut failed)?;
    (output.len()<=crate::paint::MAX_SVG_PATH_BYTES).then_some(output)
}

/// A formatter source buffer retained under the caller's geometry budget.
/// It exposes only a borrowed UTF-8 view, never an unleased String/Vec escape.
pub(crate) struct BoundedShapePath {
    bytes:lumen_common::limits::BudgetedVec<u8>,
}
impl BoundedShapePath {
    pub fn as_str(&self)->&str{core::str::from_utf8(self.bytes.as_slice()).expect("formatter writes UTF-8")}
}
struct BoundedShapeWriter {source:BoundedShapePath,failed:bool}
impl core::fmt::Write for BoundedShapeWriter {
    fn write_str(&mut self,value:&str)->core::fmt::Result{
        for byte in value.bytes(){if self.source.bytes.push(byte).is_err(){self.failed=true;return Err(core::fmt::Error);}}
        Ok(())
    }
}
#[cfg(test)]
pub(crate) fn shape_bounding_path_with_geometry_bounded(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,budget:Arc<lumen_common::limits::ByteBudget>,
)->Result<Option<BoundedShapePath>,lumen_common::svg_path::SvgGeometryLimit>{
    shape_bounding_geometry_bounded_mode(tag,attributes,user_box,geometry,css_width,css_height,budget,false,None)
}
pub(crate) fn shape_bounding_path_with_context_bounded(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,budget:Arc<lumen_common::limits::ByteBudget>,context:&crate::css::SvgCoordinateContext<'_>,
)->Result<Option<BoundedShapePath>,lumen_common::svg_path::SvgGeometryLimit>{
    shape_bounding_geometry_bounded_mode(tag,attributes,user_box,geometry,css_width,css_height,budget,true,Some(context))
}
fn shape_bounding_geometry_bounded_mode(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,budget:Arc<lumen_common::limits::ByteBudget>,computed:bool,context:Option<&crate::css::SvgCoordinateContext<'_>>,
)->Result<Option<BoundedShapePath>,lumen_common::svg_path::SvgGeometryLimit>{
    let mut output=BoundedShapeWriter{source:BoundedShapePath{bytes:lumen_common::limits::BudgetedVec::new(budget.clone(),crate::paint::MAX_SVG_PATH_BYTES)},failed:false};
    let mut failed=false;
    let valid=write_shape_geometry(tag,attributes,user_box,geometry,css_width,css_height,true,computed,context,&mut output,Some(budget),&mut failed).is_some();
    if failed||output.failed{Err(lumen_common::svg_path::SvgGeometryLimit)}else{Ok(valid.then_some(output.source))}
}
enum ShapeNumbers {
    Ordinary(Vec<f32>),Budgeted(lumen_common::limits::BudgetedVec<f32>),
}
impl ShapeNumbers {
    fn new(budget:Option<Arc<lumen_common::limits::ByteBudget>>)->Self{
        budget.map_or_else(||Self::Ordinary(Vec::new()),|budget|Self::Budgeted(lumen_common::limits::BudgetedVec::new(budget,MAX_POINTS*2)))
    }
    fn push(&mut self,value:f32)->bool{match self{Self::Ordinary(values)=>{values.push(value);true},Self::Budgeted(values)=>values.push(value).is_ok()}}
    fn as_slice(&self)->&[f32]{match self{Self::Ordinary(values)=>values,Self::Budgeted(values)=>values.as_slice()}}
}

fn write_shape_geometry(
    tag:&str,attributes:&[(Name,String)],user_box:ViewBox,geometry:&[Option<Arc<str>>;9],
    css_width:Option<f32>,css_height:Option<f32>,bounding:bool,computed:bool,context:Option<&crate::css::SvgCoordinateContext<'_>>,
    out:&mut impl core::fmt::Write,budget:Option<Arc<lumen_common::limits::ByteBudget>>,allocation_failed:&mut bool,
)->Option<()>{
    let geometry_value = |index: usize, key: &str| {
        geometry[index]
            .as_deref()
            .or_else(||if computed {Some(if index==4 || index==5 {"auto"}else{"0"})}else{attribute(attributes,key)})
    };
    let geometry_dimension = |value:&str,basis:f32| {
        if computed {coordinate_value(value,basis).map(|value|value.max(0.0))}
        else {dimension(value,basis)}
    };
    let number = |key: &str, basis: f32, default: f32| {
        if computed {return default;}

        attribute(attributes, key)
            .and_then(|value| geometry_dimension(value, basis))
            .unwrap_or(default)
    };
    let coordinate = |key: &str, index: usize, basis: f32| {
        geometry_value(index, key)
            .map(|value| {
coordinate_value(value,basis)
            })
            .unwrap_or(Some(0.0))
    };
    let attribute_coordinate = |key: &str, basis: f32| {
        attribute(attributes,key).map(|raw|context.map_or_else(||coordinate_value(raw,basis),|context|context.coordinate(raw,basis)))
            .unwrap_or(Some(0.0))
    };
    match tag {
        "rect" => {
            let (x, y, width, height) = (
                coordinate("x", 0, user_box.width)?,
                coordinate("y", 1, user_box.height)?,
                css_width
                    .or_else(|| {
                        geometry_value(2, "width")
                            .and_then(|value| geometry_dimension(value, user_box.width))
                    })
                    .unwrap_or_else(|| number("width", user_box.width, 0.0)),
                css_height
                    .or_else(|| {
                        geometry_value(3, "height")
                            .and_then(|value| geometry_dimension(value, user_box.height))
                    })
                    .unwrap_or_else(|| number("height", user_box.height, 0.0)),
            );
            if width<0.0 || height<0.0 || (!bounding && (width==0.0 || height==0.0)) {return None;}
            let rx_attribute =
                geometry_value(4, "rx").and_then(|value| geometry_dimension(value, user_box.width));
            let ry_attribute =
                geometry_value(5, "ry").and_then(|value| geometry_dimension(value, user_box.height));
            let rx = rx_attribute
                .or(ry_attribute)
                .unwrap_or(0.0)
                .min(width * 0.5);
            let ry = ry_attribute
                .or(rx_attribute)
                .unwrap_or(0.0)
                .min(height * 0.5);
            if rx == 0.0 || ry == 0.0 {
                write!(out,"M{x} {y}h{width}v{height}h-{width}Z").ok()?
            } else {
                write!(out,
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
                ).ok()?
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
                    .and_then(|value| geometry_dimension(value, basis))
                    .unwrap_or_else(|| number("r", basis, 0.0));
                (radius, radius)
            } else {
                let rx = geometry_value(4, "rx").and_then(|value| geometry_dimension(value, user_box.width));
                let ry =
                    geometry_value(5, "ry").and_then(|value| geometry_dimension(value, user_box.height));
                (rx.or(ry).unwrap_or(0.0), ry.or(rx).unwrap_or(0.0))
            };
            if rx<0.0 || ry<0.0 {return None;}
            if rx==0.0 || ry==0.0 {
                if !bounding{return None;}
                write!(out,"M{} {}L{} {}",cx-rx,cy-ry,cx+rx,cy+ry).ok()?;return Some(());
            }
            let left = cx - rx;
            let right = cx + rx;
            write!(out,"M{left} {cy}A{rx} {ry} 0 1 0 {right} {cy}A{rx} {ry} 0 1 0 {left} {cy}Z").ok()?
        }
        "line" => write!(out,
            "M{} {}L{} {}",
            attribute_coordinate("x1", user_box.width)?,
            attribute_coordinate("y1", user_box.height)?,
            attribute_coordinate("x2", user_box.width)?,
            attribute_coordinate("y2", user_box.height)?
        ).ok()?,
        "polyline" | "polygon" => {
            let mut points=ShapeNumbers::new(budget.clone());
            number_list_each(attribute(attributes,"points")?,MAX_POINTS*2,|value|{
                if points.push(value){true}else{*allocation_failed=true;false}
            })?;
            if points.as_slice().len()<(if bounding{2}else{4}) || points.as_slice().len()%2!=0{return None;}
            for (index,pair) in points.as_slice().chunks_exact(2).enumerate(){
                let (x,y)=(pair[0],pair[1]);
                write!(out,"{}{x} {y}",if index==0{'M'}else{'L'}).ok()?;
            }
            if tag=="polygon"{out.write_char('Z').ok()?;}

        }
        _ => return None,
    };
    Some(())
}

pub(crate) fn number_list(input:&str,limit:usize)->Option<Vec<f32>> {
    let mut values=Vec::new();number_list_each(input,limit,|value|{values.push(value);true})?;Some(values)
}

pub(crate) fn number_list_each(input:&str,limit:usize,mut push:impl FnMut(f32)->bool)->Option<()> {
    number_list_tokens_each(input,limit,|token|token.parse::<f32>().ok()
        .filter(|value|value.is_finite()).is_some_and(&mut push))
}

/// Maintained SVG number boundaries shared by f32 geometry and precise rotation
/// evaluation. Consumers choose their numeric representation after lexing.
fn number_list_tokens_each(input:&str,limit:usize,mut push:impl FnMut(&str)->bool)->Option<()> {
    let bytes = input.as_bytes();
    let mut count=0usize;
    let mut offset = 0;
    let mut comma_pending = false;
    while offset < bytes.len() {
        while bytes.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if bytes.get(offset) == Some(&b',') {
            if count==0 || comma_pending {
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
        if count == limit {return None;}
        if !push(input.get(start..offset)?){return None;}count+=1;
        comma_pending = false;
        if offset < bytes.len()
            && !bytes[offset].is_ascii_whitespace()
            && bytes[offset] != b','
            && !matches!(bytes[offset], b'+' | b'-')
        {
            return None;
        }
    }
    (!comma_pending).then_some(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_svg_xml_viewport_and_symbol_reference_share_owner_numeric_context() {
        let mut style=crate::css::Style::initial();style.font_size=10.0;
        let viewport=crate::css::MediaEnvironment::default();
        let context=crate::css::SvgCoordinateContext::new(&style,None,viewport,crate::css::ContainerUnitContext::no_container(viewport),(1,1));
        let parent=super::ViewBox{min_x:0.0,min_y:0.0,width:400.0,height:200.0};
        let attributes=alloc::vec![("width".into(),"calc(50% - 1em)".into()),("height".into(),"calc(50% - 2em)".into())];
        let (rect,_,user)=super::instance_viewport_with_context("svg",&attributes,&[const{None};9],None,None,parent,&context).unwrap();
        assert_eq!(rect,crate::paint::Rect{x:0.0,y:0.0,width:190.0,height:80.0});
        assert_eq!(user.width,190.0);assert_eq!(user.height,80.0);
        let attributes=alloc::vec![("viewBox".into(),"0 0 100 50".into()),("refX".into(),"1em".into()),("refY".into(),"calc(25% + 1em)".into())];
        let (rect,matrix,_)=super::instance_viewport_with_context("symbol",&attributes,&[const{None};9],Some(200.0),Some(100.0),parent,&context).unwrap();
        assert_eq!(rect,crate::paint::Rect{x:-20.0,y:-45.0,width:200.0,height:100.0});
        assert_eq!((matrix.e,matrix.f),(-20.0,-45.0));
        assert!(super::supports_symbol_reference("calc(25% + 1em)",true));
        assert!(!super::supports_symbol_reference("1s",true));
    }

    #[test]
    fn specification_computed_shape_defaults_and_radius_math_do_not_revive_xml() {
        let attributes=alloc::vec![("x".into(),"70".into()),("y".into(),"60".into()),("rx".into(),"50".into()),("ry".into(),"20".into()),("r".into(),"99".into())];
        let viewport=super::ViewBox{min_x:0.0,min_y:0.0,width:200.0,height:100.0};
        let mut geometry=core::array::from_fn(|_|None);
        let path=super::shape_path_with_computed_geometry("rect",&attributes,viewport,&geometry,Some(40.0),Some(20.0)).unwrap();
        assert_eq!(path,"M0 0h40v20h-40Z");
        geometry[4]=Some(alloc::sync::Arc::from("calc(10% + 5px)"));
        let path=super::shape_path_with_computed_geometry("rect",&attributes,viewport,&geometry,Some(40.0),Some(20.0)).unwrap();
        assert!(path.contains("A20 10"),"auto uses the other absolute radius before per-axis half-size caps");
        geometry[8]=Some(alloc::sync::Arc::from("calc(10% - 30px)"));
        assert!(super::shape_path_with_computed_geometry("circle",&attributes,viewport,&geometry,None,None).is_none(),"negative mixed radius calculation clamps to zero at used value");
        assert_eq!(super::shape_path_with_geometry("rect",&attributes,viewport,&core::array::from_fn(|_|None),Some(40.0),Some(20.0)).unwrap().starts_with("M90 60"),true,"attribute-only public helper retains its original XML authority");
    }

    #[test]
    fn specification_symbol_reference_projection_distinguishes_missing_zero_and_keywords() {
        for (references,expected) in [
            ("",crate::paint::Rect{x:0.0,y:0.0,width:200.0,height:100.0}),
            ("refX='0' refY='0'",crate::paint::Rect{x:20.0,y:40.0,width:200.0,height:100.0}),
            ("refX='center' refY='center'",crate::paint::Rect{x:-80.0,y:-10.0,width:200.0,height:100.0}),
            ("refX='100%' refY='50px'",crate::paint::Rect{x:-180.0,y:-60.0,width:200.0,height:100.0}),
        ] {
            let document=crate::html::parse(&alloc::format!("<svg><symbol id='s' viewBox='10 20 100 50' {references}></symbol></svg>"),16).unwrap();
            let node=crate::selector::query_selector(&document,document.root(),"#s").unwrap().unwrap();
            let crate::NodeKind::Element{attributes,..}=document.kind(node).unwrap() else{unreachable!()};
            let (rect,matrix,user)=super::instance_viewport_with_geometry("symbol",attributes,&[const{None};9],Some(200.0),Some(100.0),super::ViewBox{min_x:0.0,min_y:0.0,width:400.0,height:200.0}).unwrap();
            assert_eq!(rect,expected,"{references}");
            assert_eq!(user,super::ViewBox{min_x:10.0,min_y:20.0,width:100.0,height:50.0});
            assert_eq!(matrix.bounds(crate::paint::Rect{x:10.0,y:20.0,width:100.0,height:50.0}),expected,"viewport clips and content projection share the exact reference point");
        }
    }

    #[test]
    fn specification_svg_rotation_uses_precise_number_tokens_without_quarter_snapping() {
        for degrees in ["90","180","-180","450","360000000090","-360000000090"] {
            let matrix=super::parse_transform(&alloc::format!("rotate({degrees})")).unwrap();
            let (s,c)=lumen_common::dom_geometry::sin_cos_degrees(degrees.parse().unwrap());
            assert_eq!([matrix.a,matrix.b,matrix.c,matrix.d],[c as f32,s as f32,-s as f32,c as f32]);
        }
        for degrees in ["90.0000000001","89.9999999999","180.0000000001","179.9999999999"] {
            let matrix=super::parse_transform(&alloc::format!("rotate({degrees})")).unwrap();
            assert!(matrix.a!=0.0&&matrix.b!=0.0,"{degrees}: authored SVG number precision survives the shared lexer");
        }
        assert!(super::parse_transform("rotate(90,,1)").is_none());
        assert!(super::parse_transform("rotate(90 1 2 3)").is_none());
        assert!(super::parse_transform("rotate(1e100)").is_none(),"existing finite compact geometry admission remains bounded");
    }

    #[test]
    fn specification_svg_bounded_shape_formatter_preserves_geometry_and_scratch_peak() {
        use super::*;
        for shape in ["<rect x='2' y='3' width='40' height='20'/>","<rect width='30' height='20' rx='5' ry='7'/>",
            "<circle cx='20' cy='30' r='15'/>","<ellipse cx='20' cy='30' rx='15' ry='7'/>",
            "<line x1='-2' y1='4' x2='10' y2='20'/>","<polygon points='1,2 3,4 5,6'/>","<polyline points='-1,-2 3,4'/>",
            "<rect width='0' height='20'/>","<ellipse rx='0' ry='10'/>"] {
            let document=crate::xml::parse(&alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'>{shape}</svg>"),32).unwrap();
            let svg=document.first_child(document.root()).unwrap().unwrap();let node=document.first_child(svg).unwrap().unwrap();
            let crate::NodeKind::Element{name,attributes,..}=document.kind(node).unwrap() else{panic!("shape")};
            let geometry=core::array::from_fn(|_|None);let user=ViewBox{min_x:0.0,min_y:0.0,width:100.0,height:100.0};
            let ordinary=shape_bounding_path_with_geometry(local_name(name),attributes,user,&geometry,None,None).unwrap();
            let budget=lumen_common::limits::ByteBudget::new(8192);
            let bounded=shape_bounding_path_with_geometry_bounded(local_name(name),attributes,user,&geometry,None,None,budget.clone()).unwrap().unwrap();
            assert_eq!(bounded.as_str(),ordinary);assert!(budget.reserved()>0);
            let ordinary_path=lumen_common::svg_path::parse_svg_path(&ordinary).unwrap();
            let bounded_path=lumen_common::svg_path::parse_svg_path_bounded(bounded.as_str(),8192).unwrap();
            assert_eq!(ordinary_path.object_bounds(),bounded_path.object_bounds());
            drop(bounded);assert_eq!(budget.reserved(),0);
            let tiny=lumen_common::limits::ByteBudget::new(1);
            assert!(shape_bounding_path_with_geometry_bounded(local_name(name),attributes,user,&geometry,None,None,tiny.clone()).is_err());
            assert_eq!(tiny.reserved(),0);
        }
        let document=crate::xml::parse("<svg xmlns='http://www.w3.org/2000/svg'><polygon points='0 0 1 1 2 2 3 3'/></svg>",16).unwrap();
        let svg=document.first_child(document.root()).unwrap().unwrap();let node=document.first_child(svg).unwrap().unwrap();
        let crate::NodeKind::Element{attributes,..}=document.kind(node).unwrap() else{panic!("polygon")};
        let budget=lumen_common::limits::ByteBudget::new(40);let geometry=core::array::from_fn(|_|None);
        assert!(shape_bounding_path_with_geometry_bounded("polygon",attributes,ViewBox{min_x:0.0,min_y:0.0,width:10.0,height:10.0},&geometry,None,None,budget.clone()).is_err(),"number-buffer old+new growth and simultaneous formatter output share one budget");
        assert_eq!(budget.reserved(),0);
        let mut values=[0.0;4];let mut count=0;
        number_list_each("-1, .25 2e2 -3",4,|value|{values[count]=value;count+=1;true}).unwrap();
        assert_eq!(values,[-1.0,0.25,200.0,-3.0]);
        assert!(number_list_each("1 2",4,|_|false).is_none(),"bounded consumer may abort the shared lexer immediately");
    }

    #[test]
    fn specification_svg_view_fragment_atomic_grammar_shared_transform_and_live_id_resolution() {
        let fragment=Fragment::parse("svgView( preserveAspectRatio(none);viewBox(100,0,100,50);transform(translate(10,20)) )").unwrap();
        let document=crate::xml::parse("<svg xmlns='http://www.w3.org/2000/svg'><view id='actual' viewBox='0 0 20 10' preserveAspectRatio='xMaxYMax slice'/></svg>",32).unwrap();
        let spec = fragment.resolve(&document).unwrap();
        assert_eq!(
            spec.view_box,
            Some(ViewBox {
                min_x: 100.0,
                min_y: 0.0,
                width: 100.0,
                height: 50.0
            })
        );
        let (base, _) = view_box_transform_with_aspect(
            spec.view_box,
            spec.aspect.unwrap(),
            0.0,
            0.0,
            200.0,
            100.0,
        );
        let projected = spec.transform.unwrap().then(base);
        assert_eq!(projected.apply(100.0, 0.0), (10.0, 20.0));
        for invalid in [
            "svgView()",
            "svgView(viewBox(0,0,1,1);viewBox(0,0,2,2))",
            "svgView(viewBox(-1,0,-2,2))",
            "svgView(preserveAspectRatio(invalid))",
            "svgView(transform(translate(NaN)))",
            "svgView(viewBox(0,0,1,1);)",
            "svgView(viewTarget(actual))",
        ] {
            assert!(Fragment::parse(invalid).is_none(), "{invalid}");
        }
        let named = Fragment::parse("actual").unwrap();
        assert_eq!(
            named.resolve(&document).unwrap().view_box.unwrap().width,
            20.0
        );
        let mut document = document;
        let view = crate::selector::get_element_by_id(&document, document.root(), "actual")
            .unwrap()
            .unwrap();
        document
            .set_attribute(view, "viewBox", "0 0 40 10")
            .unwrap();
        assert_eq!(
            named.resolve(&document).unwrap().view_box.unwrap().width,
            40.0
        );
        assert!(Fragment::parse("x".repeat(MAX_TRANSFORM_BYTES + 1).as_str()).is_none());
        assert_eq!(
            Fragment::parse("svgView(viewBox(0,0,0,10))")
                .unwrap()
                .resolve(&document)
                .unwrap()
                .view_box
                .unwrap()
                .width,
            0.0
        );
    }

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
