//! DOM geometry snapshots backed by the last completed retained layout.
use super::*;
use lumen_html::{paint::Rect, session::RenderSession, NodeId};

/// CSSOM-facing geometry values for one element. Coordinates and lengths use
/// CSS pixels, independent of the renderer's device scale.
#[derive(Clone, Debug, PartialEq)]
pub struct GeometrySnapshot {
    pub client_rects: Vec<Rect>,
    pub has_layout_box: bool,
    pub bounding_client_rect: Rect,
    pub content_rect: Rect,
    pub offset_parent: Option<NodeId>,
    pub offset_left: f32,
    pub offset_top: f32,
    pub offset_width: f32,
    pub offset_height: f32,
    pub border_box_size: (f32, f32),
    pub client_width: f32,
    pub client_height: f32,
    pub content_width: f32,
    pub content_height: f32,
    pub scroll_width: f32,
    pub scroll_height: f32,
    pub scroll_left: f32,
    pub scroll_top: f32,
}

#[lumen_bind::class(name = "DOMRectReadOnly", hint(js(webidl)))]
pub struct DomRectReadOnly {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[lumen_bind::methods]
impl DomRectReadOnly {
    #[constructor]
    fn new(
        #[default(0.0)] x: f64,
        #[default(0.0)] y: f64,
        #[default(0.0)] width: f64,
        #[default(0.0)] height: f64,
    ) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[getter]
    fn x(&self) -> f64 {
        self.x
    }
    #[getter]
    fn y(&self) -> f64 {
        self.y
    }
    #[getter]
    fn width(&self) -> f64 {
        self.width
    }
    #[getter]
    fn height(&self) -> f64 {
        self.height
    }
    #[getter]
    fn top(&self) -> f64 {
        self.y.min(self.y + self.height)
    }
    #[getter]
    fn right(&self) -> f64 {
        self.x.max(self.x + self.width)
    }
    #[getter]
    fn bottom(&self) -> f64 {
        self.y.max(self.y + self.height)
    }
    #[getter]
    fn left(&self) -> f64 {
        self.x.min(self.x + self.width)
    }

    #[method(name = "toJSON")]
    fn to_json(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let value = Value::Obj(ctx.new_object());
        for (name, number) in [
            ("x", self.x),
            ("y", self.y),
            ("width", self.width),
            ("height", self.height),
            ("top", self.top()),
            ("right", self.right()),
            ("bottom", self.bottom()),
            ("left", self.left()),
        ] {
            ctx.set_member(&value, name, Value::Num(number))
                .map_err(|_| OpError::new("TypeError", "DOMRect serialization failed"))?;
        }
        Ok(value)
    }
}

#[lumen_bind::class(name = "DOMRect", extends = DomRectReadOnly, hint(js(webidl)))]
pub struct DomRect {
    base: DomRectReadOnly,
}

#[lumen_bind::methods]
impl DomRect {
    #[constructor]
    fn new(
        #[default(0.0)] x: f64,
        #[default(0.0)] y: f64,
        #[default(0.0)] width: f64,
        #[default(0.0)] height: f64,
    ) -> Self {
        Self {
            base: DomRectReadOnly {
                x,
                y,
                width,
                height,
            },
        }
    }
    #[getter]
    fn x(&self) -> f64 {
        self.base.x
    }
    #[setter]
    fn set_x(&mut self, value: f64) {
        self.base.x = value;
    }
    #[getter]
    fn y(&self) -> f64 {
        self.base.y
    }
    #[setter]
    fn set_y(&mut self, value: f64) {
        self.base.y = value;
    }
    #[getter]
    fn width(&self) -> f64 {
        self.base.width
    }
    #[setter]
    fn set_width(&mut self, value: f64) {
        self.base.width = value;
    }
    #[getter]
    fn height(&self) -> f64 {
        self.base.height
    }
    #[setter]
    fn set_height(&mut self, value: f64) {
        self.base.height = value;
    }
}

#[lumen_bind::class(name = "DOMRectList", hint(js(webidl)))]
pub struct DomRectList {
    rects: Vec<Rect>,
}
#[lumen_bind::methods]
impl DomRectList {
    #[constructor]
    fn new() -> Self {
        Self { rects: Vec::new() }
    }
    #[getter]
    fn length(&self) -> usize {
        self.rects.len()
    }
    fn item(&self, ctx: &mut Ctx, index: usize) -> Value {
        self.rects
            .get(index)
            .copied()
            .map_or(Value::Null, |rect| rect_value(ctx, rect))
    }
}

pub fn rect_value(ctx: &mut Ctx, rect: Rect) -> Value {
    ctx.new_instance(DomRect {
        base: DomRectReadOnly {
            x: f64::from(rect.x),
            y: f64::from(rect.y),
            width: f64::from(rect.width),
            height: f64::from(rect.height),
        },
    })
}

pub fn readonly_rect_value(ctx: &mut Ctx, rect: Rect) -> Value {
    ctx.new_instance(DomRectReadOnly {
        x: f64::from(rect.x),
        y: f64::from(rect.y),
        width: f64::from(rect.width),
        height: f64::from(rect.height),
    })
}

pub fn client_rects_value(ctx: &mut Ctx, rects: &[Rect]) -> Value {
    let list = ctx.new_instance(DomRectList {
        rects: rects.to_vec(),
    });
    for (index, rect) in rects.iter().copied().enumerate() {
        let value = rect_value(ctx, rect);
        let _ = ctx.set_member(&list, &index.to_string(), value);
    }
    list
}

pub fn bounding_client_rect_value(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> Value {
    let rect = snapshot(&mut realm.session.borrow_mut(), node)
        .map(|geometry| geometry.bounding_client_rect)
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        });
    rect_value(ctx, rect)
}

pub fn client_rect_list_value(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> Value {
    let rects = realm.session.borrow().client_rects(node);
    client_rects_value(ctx, &rects)
}

pub fn install(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    let readonly = ctx.class_constructor::<DomRectReadOnly>();
    let rect = ctx.class_constructor::<DomRect>();
    let list = ctx.class_constructor::<DomRectList>();
    crate::install_interface(ctx, &global, "DOMRectReadOnly", readonly)
        .map_err(|_| OpError::new("Error", "DOMRectReadOnly installation failed"))?;
    crate::install_interface(ctx, &global, "DOMRect", rect)
        .map_err(|_| OpError::new("Error", "DOMRect installation failed"))?;
    crate::install_interface(ctx, &global, "DOMRectList", list)
        .map_err(|_| OpError::new("Error", "DOMRectList installation failed"))?;
    Ok(())
}

fn union(rects: &[Rect]) -> Rect {
    let Some(first) = rects.first().copied() else {
        return Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
    };
    let (mut left, mut top, mut right, mut bottom) = (
        first.x,
        first.y,
        first.x + first.width,
        first.y + first.height,
    );
    for rect in &rects[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// Return the border widths that participate in the rendered box. The CSS
/// `border-width` initial value is medium (represented as three CSS pixels in
/// this renderer), but it does not affect layout while the corresponding
/// border style is `none`.
fn used_border_widths(style: &lumen_html::css::Style) -> [f32; 4] {
    let widths = style.border_width_sides();
    let solids = style.border_solid_sides;
    core::array::from_fn(|side| {
        if solids[side].unwrap_or(style.border_solid) {
            widths[side]
        } else {
            0.0
        }
    })
}

/// Read layout geometry without forcing a layout pass. A missing snapshot means
/// the renderer has not completed layout for the current document version.
pub fn snapshot(session: &mut RenderSession, node: NodeId) -> Option<GeometrySnapshot> {
    session.viewport_size()?;
    if !matches!(
        session.document().kind(node),
        Ok(lumen_html::NodeKind::Element { .. })
    ) {
        return None;
    }
    let root = session.document().root();
    let mut current = node;
    let mut connected = true;
    while current != root {
        let Some(parent) = session.document().composed_parent(current).ok().flatten() else {
            connected = false;
            break;
        };
        current = parent;
    }
    let client_rects = session.client_rects(node);
    let border_box = union(&client_rects);
    let layout_box = session.layout_rect(node).unwrap_or(border_box);
    let style = session.computed_style(node).ok()?;
    let padding = style.padding_sides();
    let border = used_border_widths(&style);
    let client_width = (layout_box.width - border[1] - border[3]).max(0.0);
    let client_height = (layout_box.height - border[0] - border[2]).max(0.0);
    let content_width = (client_width - padding[1] - padding[3]).max(0.0);
    let content_height = (client_height - padding[0] - padding[2]).max(0.0);
    let (overflow_x, overflow_y) = session.scroll_extent(node).unwrap_or((0.0, 0.0));
    let (scroll_left, scroll_top) = session.scroll_offset(node);

    let mut ancestor = session.document().composed_parent(node).ok().flatten();
    let mut offset_parent = None;
    while let Some(parent) = ancestor {
        if matches!(
            session.document().kind(parent),
            Ok(lumen_html::NodeKind::Element { .. })
        ) && session
            .computed_style(parent)
            .is_ok_and(|style| style.position != lumen_html::css::Position::Static)
        {
            offset_parent = Some(parent);
            break;
        }
        ancestor = session.document().composed_parent(parent).ok().flatten();
    }
    let (offset_left, offset_top) = offset_parent
        .and_then(|parent| session.layout_rect(parent).map(|rect| (rect.x, rect.y)))
        .map_or((border_box.x, border_box.y), |(x, y)| {
            let parent_style = session
                .computed_style(offset_parent.expect("set above"))
                .ok();
            let parent_border = parent_style.as_ref().map_or([0.0; 4], used_border_widths);
            (
                layout_box.x - x - parent_border[3],
                layout_box.y - y - parent_border[0],
            )
        });

    Some(GeometrySnapshot {
        has_layout_box: connected && !client_rects.is_empty(),
        client_rects,
        bounding_client_rect: border_box,
        content_rect: Rect {
            x: layout_box.x + border[3] + padding[3],
            y: layout_box.y + border[0] + padding[0],
            width: content_width,
            height: content_height,
        },
        offset_parent,
        offset_left,
        offset_top,
        offset_width: layout_box.width.round(),
        offset_height: layout_box.height.round(),
        border_box_size: (layout_box.width, layout_box.height),
        client_width: client_width.round(),
        client_height: client_height.round(),
        content_width,
        content_height,
        scroll_width: (client_width + overflow_x.max(0.0)).round(),
        scroll_height: (client_height + overflow_y.max(0.0)).round(),
        scroll_left,
        scroll_top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::{
        html,
        paint::{ShapedRun, TextShaper},
    };

    struct NoText;
    impl TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    #[test]
    fn geometry_uses_rendered_css_pixel_boxes_and_invalidates_with_dom() {
        let document = html::parse("<style>body{margin:0}#outer{position:relative;width:60px;height:40px;padding:4px;border:2px solid;overflow:auto}#inner{width:120px;height:80px}</style><div id=outer><div id=inner></div></div>", 128).unwrap();
        let outer = lumen_html::selector::query_selector(&document, document.root(), "#outer")
            .unwrap()
            .unwrap();
        let inner = lumen_html::selector::query_selector(&document, document.root(), "#inner")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(200, 150, &NoText).unwrap();
        let outer_geometry = snapshot(&mut session, outer).unwrap();
        let inner_geometry = snapshot(&mut session, inner).unwrap();
        assert_eq!(outer_geometry.offset_width, 72.0);
        assert_eq!(outer_geometry.client_width, 68.0);
        assert_eq!(outer_geometry.scroll_width, 128.0);
        assert_eq!(inner_geometry.offset_parent, Some(outer));
        let before_scroll_y = inner_geometry.bounding_client_rect.y;
        assert!(session.set_scroll_offset(outer, 0.0, 10.0).unwrap());
        assert_eq!(snapshot(&mut session, outer).unwrap().scroll_top, 10.0);
        assert_eq!(
            snapshot(&mut session, inner)
                .unwrap()
                .bounding_client_rect
                .y,
            before_scroll_y - 10.0
        );
        session
            .document_mut()
            .set_attribute(inner, "class", "changed")
            .unwrap();
        assert!(snapshot(&mut session, inner).is_none());
    }

    #[test]
    fn transformed_client_bounds_keep_untransformed_offset_dimensions() {
        let document = html::parse("<style>body{margin:0}#target{width:20px;height:10px;transform:scale(2);transform-origin:0 0}</style><div id=target></div>", 32).unwrap();
        let target = lumen_html::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        let geometry = snapshot(&mut session, target).unwrap();
        assert_eq!(geometry.bounding_client_rect.width, 40.0);
        assert_eq!(geometry.bounding_client_rect.height, 20.0);
        assert_eq!(geometry.offset_width, 20.0);
        assert_eq!(geometry.offset_height, 10.0);
    }
}
