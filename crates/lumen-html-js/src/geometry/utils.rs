//! CSSOM View GeometryUtils: `getBoxQuads()` and the `convert*FromNode()` family.
use super::value_types::{new_point, new_quad, point_dictionary, quad_init, rect_dictionary};
use super::*;
use lumen_html::paint::Affine;

#[derive(Clone, Copy, PartialEq)]
enum CssBox {
    Margin,
    Border,
    Padding,
    Content,
}

#[derive(Clone, Copy)]
pub(crate) enum Subject {
    Element(NodeId),
    Text(NodeId),
    Document,
}

struct Edges {
    margin: [f32; 4],
    border: [f32; 4],
    padding: [f32; 4],
    inline: bool,
    rtl: bool,
}

impl Edges {
    fn none() -> Self {
        Self { margin: [0.0; 4], border: [0.0; 4], padding: [0.0; 4], inline: false, rtl: false }
    }

    /// Which physical sides carry the box edges for fragment `index` of `count`:
    /// an inline box breaks only its start edge onto the first fragment and its end
    /// edge onto the last one.
    fn sides(&self, index: usize, count: usize) -> (bool, bool) {
        if !self.inline {
            return (true, true);
        }
        let (first, last) = (index == 0, index + 1 == count);
        if self.rtl { (last, first) } else { (first, last) }
    }

    fn box_rect(&self, kind: CssBox, rect: Rect, left_on: bool, right_on: bool) -> Rect {
        let side = |values: [f32; 4]| [values[0], if right_on { values[1] } else { 0.0 }, values[2], if left_on { values[3] } else { 0.0 }];
        let (margin, border, padding) = (side(self.margin), side(self.border), side(self.padding));
        let inset: [f32; 4] = match kind {
            CssBox::Margin => margin.map(|value| -value),
            CssBox::Border => [0.0; 4],
            CssBox::Padding => border,
            CssBox::Content => core::array::from_fn(|index| border[index] + padding[index]),
        };
        Rect {
            x: rect.x + inset[3],
            y: rect.y + inset[0],
            width: (rect.width - inset[1] - inset[3]).max(0.0),
            height: (rect.height - inset[0] - inset[2]).max(0.0),
        }
    }
}

struct Boxes {
    fragments: Vec<(Rect, Vec<Affine>)>,
    edges: Edges,
}

impl Boxes {
    fn new(fragments: Vec<(Rect, Vec<Affine>)>, edges: Edges) -> Option<Self> {
        (!fragments.is_empty()).then_some(Self { fragments, edges })
    }

    fn forward(transforms: &[Affine]) -> Affine {
        transforms.iter().fold(Affine::IDENTITY, |accumulated, next| accumulated.then(*next))
    }

    fn quad(&self, kind: CssBox, index: usize) -> [(f32, f32); 4] {
        let (rect, transforms) = &self.fragments[index];
        let (left_on, right_on) = self.edges.sides(index, self.fragments.len());
        let r = self.edges.box_rect(kind, *rect, left_on, right_on);
        let matrix = Self::forward(transforms);
        [(r.x, r.y), (r.x + r.width, r.y), (r.x + r.width, r.y + r.height), (r.x, r.y + r.height)]
            .map(|(x, y)| matrix.apply(x, y))
    }

    /// Offset of a box's top-left corner from the first fragment's border-box origin.
    fn box_origin(&self, kind: CssBox) -> (f32, f32) {
        let (rect, _) = &self.fragments[0];
        let (left_on, right_on) = self.edges.sides(0, self.fragments.len());
        let r = self.edges.box_rect(kind, *rect, left_on, right_on);
        (r.x - rect.x, r.y - rect.y)
    }

    /// Viewport coordinates to the border-box coordinate space of this node.
    fn to_local(&self, point: (f32, f32)) -> Option<(f32, f32)> {
        let (rect, transforms) = &self.fragments[0];
        let (x, y) = Self::forward(transforms).inverse()?.apply(point.0, point.1);
        Some((x - rect.x, y - rect.y))
    }

    fn from_local(&self, point: (f32, f32)) -> (f32, f32) {
        let (rect, transforms) = &self.fragments[0];
        Self::forward(transforms).apply(point.0 + rect.x, point.1 + rect.y)
    }
}

fn dictionary_object(options: &Option<Value>) -> OpResult<Option<&Value>> {
    match options {
        None | Some(Value::Undefined | Value::Null) => Ok(None),
        Some(value @ Value::Obj(_)) => Ok(Some(value)),
        Some(_) => Err(OpError::type_error("Expected a dictionary")),
    }
}

fn css_box_member(ctx: &mut Ctx, options: Option<&Value>, name: &str) -> OpResult<CssBox> {
    let Some(options) = options else { return Ok(CssBox::Border) };
    let value = ctx.member_get(options, name).map_err(OpError::thrown)?;
    if matches!(value, Value::Undefined) {
        return Ok(CssBox::Border);
    }
    let text = ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string();
    match text.as_str() {
        "margin" => Ok(CssBox::Margin),
        "border" => Ok(CssBox::Border),
        "padding" => Ok(CssBox::Padding),
        "content" => Ok(CssBox::Content),
        _ => Err(OpError::type_error("Invalid CSSBoxType")),
    }
}

fn subject_of(ctx: &mut Ctx, value: &Value) -> OpResult<(Rc<DomRealm>, Subject)> {
    let (realm, id) = ctx
        .with_instance::<DomNode, _>(value, |node| (node.realm.clone(), node.id))
        .map_err(|_| OpError::type_error("Expected a Text, Element or Document"))?;
    let subject = match realm.session.borrow().document().kind(id) {
        Ok(NodeKind::Element { .. }) => Subject::Element(id),
        Ok(NodeKind::Text(_) | NodeKind::CData(_)) => Subject::Text(id),
        Ok(NodeKind::Document) => Subject::Document,
        _ => return Err(OpError::type_error("Expected a Text, Element or Document")),
    };
    Ok((realm, subject))
}

fn prepare(ctx: &mut Ctx, realm: &Rc<DomRealm>, subject: Subject) -> OpResult<()> {
    let node = match subject {
        Subject::Element(id) => Some(id),
        _ => None,
    };
    animations::flush_css_transitions_for_property(ctx, realm, node, Some("width"))?;
    if matches!(subject, Subject::Text(_)) {
        realm.session.borrow_mut().set_selectable_text(true);
    }
    realm.flush_layout()
}

fn boxes_of(realm: &Rc<DomRealm>, subject: Subject) -> Option<Boxes> {
    let mut session = realm.session.borrow_mut();
    match subject {
        Subject::Element(id) => {
            let fragments = session.client_fragment_boxes(id);
            if fragments.is_empty() {
                return None;
            }
            let edges = match session.used_box_style(id) {
                Ok(style) => Edges {
                    margin: session.layout_used_margins(id).unwrap_or(style.margin_sides),
                    border: style.used_border_widths(),
                    padding: style.padding_sides(),
                    inline: style.display == lumen_html::css::Display::Inline,
                    rtl: style.used_direction() == lumen_html::css::Direction::Rtl,
                },
                Err(_) => Edges::none(),
            };
            Boxes::new(fragments, edges)
        }
        Subject::Text(id) => Boxes::new(
            session.text_node_client_rects(id).into_iter().map(|rect| (rect, Vec::new())).collect(),
            Edges::none(),
        ),
        Subject::Document => {
            let root = lumen_html::selector::document_element(session.document());
            let size = root.and_then(|root| snapshot(&mut session, root)).map(|snapshot| (snapshot.client_width, snapshot.client_height));
            let fragments = size
                .map(|(width, height)| vec![(Rect { x: 0.0, y: 0.0, width, height }, Vec::new())])
                .unwrap_or_default();
            Boxes::new(fragments, Edges::none())
        }
    }
}

fn not_found(message: &'static str) -> OpError {
    OpError::new("NotFoundError", message)
}

pub(crate) fn get_box_quads(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    subject: Subject,
    options: Option<Value>,
) -> OpResult<Vec<Value>> {
    let options = dictionary_object(&options)?;
    let kind = css_box_member(ctx, options, "box")?;
    let relative = match options {
        Some(options) => {
            let value = ctx.member_get(options, "relativeTo").map_err(OpError::thrown)?;
            if matches!(value, Value::Undefined) { None } else { Some(subject_of(ctx, &value)?) }
        }
        None => None,
    };
    prepare(ctx, realm, subject)?;
    let boxes = boxes_of(realm, subject);
    let relative_boxes = match &relative {
        Some((relative_realm, relative_subject)) => {
            if !Rc::ptr_eq(relative_realm, realm) {
                return Err(not_found("relativeTo belongs to another document"));
            }
            prepare(ctx, relative_realm, *relative_subject)?;
            let target = boxes_of(relative_realm, *relative_subject)
                .ok_or_else(|| not_found("relativeTo has no associated box"))?;
            Some(target)
        }
        None => None,
    };
    let Some(boxes) = boxes else { return Ok(Vec::new()) };
    let mut quads = Vec::with_capacity(boxes.fragments.len());
    for index in 0..boxes.fragments.len() {
        let mut corners = boxes.quad(kind, index);
        if let Some(target) = &relative_boxes {
            for corner in &mut corners {
                *corner = target.to_local(*corner).ok_or_else(|| not_found("relativeTo is not invertible"))?;
            }
        }
        quads.push(new_quad(ctx, corners.map(|(x, y)| [f64::from(x), f64::from(y)])));
    }
    Ok(quads)
}

fn convert_points(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    subject: Subject,
    from: &Value,
    options: Option<Value>,
    points: &[[f64; 2]],
) -> OpResult<Vec<[f64; 2]>> {
    let options = dictionary_object(&options)?;
    let from_kind = css_box_member(ctx, options, "fromBox")?;
    let to_kind = css_box_member(ctx, options, "toBox")?;
    let (from_realm, from_subject) = subject_of(ctx, from)?;
    prepare(ctx, realm, subject)?;
    prepare(ctx, &from_realm, from_subject)?;
    if !Rc::ptr_eq(realm, &from_realm) {
        return Err(not_found("The source node belongs to another document"));
    }
    let (Some(target), Some(source)) = (boxes_of(realm, subject), boxes_of(&from_realm, from_subject)) else {
        return Err(not_found("The node has no associated box"));
    };
    let source_origin = source.box_origin(from_kind);
    let target_origin = target.box_origin(to_kind);
    points
        .iter()
        .map(|point| {
            let viewport = source.from_local((point[0] as f32 + source_origin.0, point[1] as f32 + source_origin.1));
            let local = target.to_local(viewport).ok_or_else(|| not_found("The node is not invertible"))?;
            Ok([f64::from(local.0 - target_origin.0), f64::from(local.1 - target_origin.1)])
        })
        .collect()
}

pub(crate) fn convert_quad(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    subject: Subject,
    quad: Value,
    from: Value,
    options: Option<Value>,
) -> OpResult<Value> {
    let corners = quad_init(ctx, &quad)?;
    let converted = convert_points(ctx, realm, subject, &from, options, &corners)?;
    Ok(new_quad(ctx, [converted[0], converted[1], converted[2], converted[3]]))
}

pub(crate) fn convert_rect(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    subject: Subject,
    rect: Value,
    from: Value,
    options: Option<Value>,
) -> OpResult<Value> {
    let [x, y, width, height] = rect_dictionary(ctx, &rect)?;
    let corners = [[x, y], [x + width, y], [x + width, y + height], [x, y + height]];
    let converted = convert_points(ctx, realm, subject, &from, options, &corners)?;
    Ok(new_quad(ctx, [converted[0], converted[1], converted[2], converted[3]]))
}

pub(crate) fn convert_point(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    subject: Subject,
    point: Value,
    from: Value,
    options: Option<Value>,
) -> OpResult<Value> {
    let point = point_dictionary(ctx, &point)?;
    let converted = convert_points(ctx, realm, subject, &from, options, &[[point.0[0], point.0[1]]])?;
    Ok(new_point(ctx, converted[0][0], converted[0][1]))
}

fn prepare_text_layout(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    animations::flush_css_transitions_for_property(ctx, realm, None, Some("width"))?;
    realm.session.borrow_mut().set_selectable_text(true);
    realm.flush_layout()
}

/// The caret at a text boundary: a zero-width rectangle as tall as the adjacent character.
pub(crate) fn collapsed_range_rect(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    point: lumen_html::ranges::Boundary,
) -> OpResult<Option<Rect>> {
    prepare_text_layout(ctx, realm)?;
    let session = realm.session.borrow();
    let Ok(NodeKind::Text(text) | NodeKind::CData(text)) = session.document().kind(point.container) else {
        return Ok(None);
    };
    let length = text.encode_utf16().count();
    let boundary = |offset| lumen_html::ranges::Boundary { container: point.container, offset };
    if point.offset < length {
        let rects = session.range_client_rects(point, boundary(point.offset + 1));
        return Ok(rects.first().map(|rect| Rect { x: rect.x, y: rect.y, width: 0.0, height: rect.height }));
    }
    if point.offset > 0 {
        let rects = session.range_client_rects(boundary(point.offset - 1), point);
        return Ok(rects.last().map(|rect| Rect { x: rect.x + rect.width, y: rect.y, width: 0.0, height: rect.height }));
    }
    Ok(None)
}

pub(crate) fn range_client_rects(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    start: lumen_html::ranges::Boundary,
    end: lumen_html::ranges::Boundary,
) -> OpResult<Vec<Rect>> {
    if start == end {
        return Ok(collapsed_range_rect(ctx, realm, start)?.into_iter().collect());
    }
    prepare_text_layout(ctx, realm)?;
    Ok(realm.session.borrow().range_client_rects(start, end))
}
