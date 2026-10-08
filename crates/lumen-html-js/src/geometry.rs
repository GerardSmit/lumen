//! DOM geometry snapshots backed by the last completed retained layout.
use super::*;
use lumen_html::{paint::Rect, session::RenderSession, NodeId};
mod value_types;
pub(crate) use value_types::{matrix_native_value, matrix_native_instance, validate_mutable_matrix};

/// Resolve one visible element without allocating a result list or dedup table.
pub(crate) fn element_from_point_in_tree(
    realm: &Rc<DomRealm>,
    tree_root: Option<NodeId>,
    x: f64,
    y: f64,
) -> OpResult<Option<NodeId>> {
    let mut found = None;
    visit_elements_from_point(realm, tree_root, x, y, |node, _| {
        found = Some(node);
        false
    })?;
    Ok(found)
}

pub(crate) fn elements_from_point(realm: &Rc<DomRealm>, x: f64, y: f64) -> OpResult<Vec<NodeId>> {
    elements_from_point_in_tree(realm, None, x, y)
}

/// Resolve retained paint hits without cloning the layout or hit regions.
pub(crate) fn elements_from_point_in_tree(
    realm: &Rc<DomRealm>,
    tree_root: Option<NodeId>,
    x: f64,
    y: f64,
) -> OpResult<Vec<NodeId>> {
    let mut elements = Vec::new();
    let mut seen = std::collections::HashSet::new();
    visit_elements_from_point(realm, tree_root, x, y, |node, root_fallback| {
        if if root_fallback {
            elements.last() != Some(&node)
        } else {
            seen.insert(node)
        } {
            elements.push(node);
        }
        true
    })?;
    Ok(elements)
}

/// Shared validation, layout, shadow retargeting and root fallback. A false
/// visitor result stops immediately, including before the root fallback.
fn visit_elements_from_point(
    realm: &Rc<DomRealm>,
    tree_root: Option<NodeId>,
    x: f64,
    y: f64,
    mut visit: impl FnMut(NodeId, bool) -> bool,
) -> OpResult<()> {
    if !x.is_finite() || !y.is_finite() {
        return Err(OpError::type_error("Point coordinates must be finite"));
    }
    if x < 0.0 || y < 0.0 {
        return Ok(());
    }
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok(());
    }
    realm.flush_layout()?;
    let session = realm.session.borrow();
    let document = session.document();
    let Some((width, height)) = session.viewport_size() else {
        return Ok(());
    };
    if x > f64::from(width) || y > f64::from(height) {
        return Ok(());
    }
    if realm.browser_services.render_capture.suppress_hit_testing.get() {
        if let Some(root) = selector::document_element(document) { visit(root, true); }
        return Ok(());
    }
    let mut stopped = false;

    session.for_each_hit_test(x as f32, y as f32, |node| {
        let Ok(node) = document.retarget(node, Some(tree_root.unwrap_or(document.root()))) else {
            return true;
        };
        if matches!(document.kind(node), Ok(NodeKind::Element { .. })) {
            stopped = !visit(node, false);
        }
        !stopped
    });
    if !stopped {
        if let Some(root) = lumen_html::selector::document_element(document) {
            visit(root, true);
        }
    }
    Ok(())
}

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
    let rect = realm
        .session
        .borrow()
        .bounding_client_rect(node)
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
    value_types::install(ctx, true)
}

pub(crate) fn install_worker(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    let readonly = ctx.class_constructor::<DomRectReadOnly>();
    let rect = ctx.class_constructor::<DomRect>();
    crate::install_interface(ctx, &global, "DOMRectReadOnly", readonly).map_err(OpError::thrown)?;
    crate::install_interface(ctx, &global, "DOMRect", rect).map_err(OpError::thrown)?;
    value_types::install(ctx, false)
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
fn used_border_widths(style: &lumen_html::css::Style) -> [f32; 4] { style.used_border_widths() }

#[derive(Clone, Copy)]
struct BoxMetrics {
    border: [f32; 4],
    padding: [f32; 4],
    client_width: f32,
    client_height: f32,
    content_width: f32,
    content_height: f32,
}

fn box_metrics(layout_box: Rect, style: &lumen_html::css::Style) -> BoxMetrics {
    let border = used_border_widths(style);
    let padding = style.padding_sides();
    let client_width = (layout_box.width - border[1] - border[3]).max(0.0);
    let client_height = (layout_box.height - border[0] - border[2]).max(0.0);
    BoxMetrics {
        border,
        padding,
        client_width,
        client_height,
        content_width: (client_width - padding[1] - padding[3]).max(0.0),
        content_height: (client_height - padding[0] - padding[2]).max(0.0),
    }
}

/// The untransformed content-box size from the current layout snapshot.
/// Unlike `snapshot`, this avoids materializing client rects and is useful to
/// host code that needs a replaced element's viewport dimensions.
pub(crate) fn content_box_size(session: &mut RenderSession, node: NodeId) -> Option<(f32, f32)> {
    let style = session.used_box_style(node).ok()?;
    if style.display == lumen_html::css::Display::None {
        return Some((0.0, 0.0));
    }
    let layout_box = session.layout_rect(node)?;
    let metrics = box_metrics(layout_box, &style);
    Some((metrics.content_width, metrics.content_height))
}

/// The target's padding-edge origin in untransformed viewport coordinates.
/// MouseEvent offsets use the retained box directly and need no rect-list allocation.
pub(crate) fn event_padding_origin(
    session: &mut RenderSession,
    mut node: NodeId,
) -> Option<(f32, f32)> {
    if matches!(
        session.document().kind(node),
        Ok(NodeKind::Element {
            namespace: lumen_html::Namespace::Svg,
            ..
        })
    ) {
        let mut current = Some(node);
        while let Some(candidate) = current {
            match session.document().kind(candidate).ok()? {
                NodeKind::Element {
                    namespace: lumen_html::Namespace::Svg,
                    name,
                    ..
                } => {
                    if name.as_str() == "svg" {
                        node = candidate;
                    }
                }
                _ => break,
            }
            current = session.document().composed_parent(candidate).ok()?;
        }
    }
    let rect = session.layout_rect(node)?;
    let style = session.computed_style(node).ok()?;
    let border = used_border_widths(&style);
    Some((rect.x + border[3], rect.y + border[0]))
}

/// Read layout geometry without forcing a layout pass. A missing snapshot means
/// the renderer has not completed layout for the current document version.
pub fn snapshot(session: &mut RenderSession, node: NodeId) -> Option<GeometrySnapshot> {
    let viewport = session.viewport_size()?;
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
    let associated_box = session.layout_rect(node);
    let layout_box = associated_box.unwrap_or(border_box);
    let style = session.used_box_style(node).ok()?;
    let metrics = box_metrics(layout_box, &style);
    // CSSOM View client dimensions use the viewport for the standards root
    // and quirks BODY, independently of their actual CSS box dimensions.
    let client_size = if !connected || associated_box.is_none() || style.display == lumen_html::css::Display::Inline {
        (0.0, 0.0)
    } else if if session.document().document_mode() == lumen_html::DocumentMode::Quirks {
        scrolling::html_body(session.document()) == Some(node)
    } else {
        selector::document_element(session.document()) == Some(node)
    } {
        (viewport.0 as f32, viewport.1 as f32)
    } else {
        (metrics.client_width.round(), metrics.client_height.round())
    };
    let (overflow_x, overflow_y) = session.scroll_extent(node).unwrap_or((0.0, 0.0));
    // A fieldset scrolls its anonymous content box below the rendered legend.
    // Its principal client box still includes the legend's allocated border.
    let scroll_viewport = if matches!(session.document().kind(node),
        Ok(lumen_html::NodeKind::Element { namespace: lumen_html::Namespace::Html, name, .. })
        if name.as_str() == "fieldset")
    { session.scrollport_size(node) } else { None };
    let (scroll_client_width, scroll_client_height) = scroll_viewport
        .unwrap_or((metrics.client_width, metrics.client_height));
    let mut scroll_size = (
        (scroll_client_width + overflow_x.max(0.0)).round(),
        (scroll_client_height + overflow_y.max(0.0)).round(),
    );
    let quirks = session.document().document_mode() == lumen_html::DocumentMode::Quirks;
    let viewport_axes = if !quirks && selector::document_element(session.document()) == Some(node) {
        (true, true)
    } else if quirks && scrolling::html_body(session.document()) == Some(node) {
        let (x, y) = scrolling::potentially_scrollable_axes_in_session(session, node, false).ok()?;
        (!x, !y)
    } else { (false, false) };
    if viewport_axes.0 || viewport_axes.1 {
        let extent = session.scroll_extent(root).unwrap_or((0.0, 0.0));
        if viewport_axes.0 { scroll_size.0 = (viewport.0 as f32 + extent.0.max(0.0)).round(); }
        if viewport_axes.1 { scroll_size.1 = (viewport.1 as f32 + extent.1.max(0.0)).round(); }
    }
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
            x: layout_box.x + metrics.border[3] + metrics.padding[3],
            y: layout_box.y + metrics.border[0] + metrics.padding[0],
            width: metrics.content_width,
            height: metrics.content_height,
        },
        offset_parent,
        offset_left,
        offset_top,
        offset_width: layout_box.width.round(),
        offset_height: layout_box.height.round(),
        border_box_size: (layout_box.width, layout_box.height),
        client_width: client_size.0,
        client_height: client_size.1,
        content_width: metrics.content_width,
        content_height: metrics.content_height,
        scroll_width: scroll_size.0,
        scroll_height: scroll_size.1,
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
    fn specification_window_client_dimensions_use_viewport_root_and_quirks_body() {
        for (doctype, viewport_owner) in [("<!doctype html>", "document.documentElement"), ("", "document.body")] {
            let mut engine = lumen::Engine::new();
            let source = format!("{doctype}<style>html{{width:400px;height:900px;border:7px solid;padding:11px;transform:scale(2)}}body{{margin:0;width:200px;height:300px}}#box{{width:20px;height:30px;padding:5px;border:3px solid;transform:scale(2)}}#inline{{padding:5px;border:3px solid}}</style><div id=box></div><span id=inline></span>");
            let realm = super::super::install(engine.ctx(), &source, 128).unwrap();
            realm.set_layout_flusher(Rc::new(|session| session.display_list(80, 60, &NoText)
                .map(|_|()).map_err(|error|format!("{error:?}"))));
            let result = engine.eval_value(&format!(r#"(() => {{
                const owner={viewport_owner};
                if(owner.clientWidth!==80||owner.clientHeight!==60)throw Error('viewport client dimensions');
                if(document.documentElement.offsetWidth!==436)throw Error('root keeps actual CSS border box');
                const box=document.getElementById('box');
                if(box.clientWidth!==30||box.clientHeight!==40||box.offsetWidth!==36)throw Error('ordinary unscaled padding box');
                const inline=document.getElementById('inline');
                if(inline.clientWidth!==0||inline.clientHeight!==0)throw Error('inline box client dimensions');
                owner.style.display='none';
                if(owner.clientWidth!==0||owner.clientHeight!==0)throw Error('no associated box precedes viewport rule');
                return true;
            }})()"#)).unwrap();
            let value=result.unwrap_or_else(|error|match engine.describe_throw(error) {
                lumen::Completion::Throw {name,message}=>panic!("client-dimension guard: {name}: {message}"),
                _=>unreachable!("describe_throw returns a throw completion"),
            });
            assert!(matches!(value,Value::Bool(true)),"client-dimension guard must return true");
        }
    }

    #[test]
    fn specification_window_scroll_dimensions_use_viewport_area_without_double_counting_root_box() {
        for doctype in ["<!doctype html>", ""] {
            let mut engine=lumen::Engine::new();
            let source=format!("{doctype}<style>html{{width:240px;height:300px}}body{{margin:0;width:80px;height:30px}}#child{{height:100px}}</style><div id=child></div>");
            let realm=super::super::install(engine.ctx(),&source,128).unwrap();
            realm.set_layout_flusher(Rc::new(|session|session.display_list(80,60,&NoText)
                .map(|_|()).map_err(|error|format!("{error:?}"))));
            let result=engine.eval_value(r#"(() => {
                const root=document.documentElement,body=document.body;
                const quirks=document.compatMode==='BackCompat';
                const owner=quirks?body:root;
                if(owner.scrollWidth!==240||owner.scrollHeight!==300)throw Error('viewport scroll area is counted once');
                if(owner.clientWidth!==80||owner.clientHeight!==60)throw Error('viewport client size');
                root.style.overflow='hidden';body.style.overflow='hidden';
                if(body.scrollHeight!==100)throw Error('potentially scrollable BODY uses its own area');
                if(body.clientHeight!==(quirks?60:30))throw Error('client BODY rule remains independent of scroll area');
                return true;
            })()"#).unwrap();
            let value=result.unwrap_or_else(|error|match engine.describe_throw(error) {
                lumen::Completion::Throw {name,message}=>panic!("scroll-dimension guard: {name}: {message}"),
                _=>unreachable!("describe_throw returns a throw completion"),
            });
            assert!(matches!(value,Value::Bool(true)),"scroll-dimension guard must return true");
        }
    }

    #[test]
    fn specification_positioned_scroll_area_uses_actual_containing_block_without_flow_width() {
        for (source, assertions) in [
            ("<!doctype html><style>html,body{margin:0}#box{position:fixed;width:160px;height:120px}</style><div id=box></div>",
             "document.documentElement.scrollWidth===80&&document.documentElement.scrollHeight===60"),
            ("<!doctype html><style>html,body{margin:0}body{width:80px;height:60px;transform:translate(0)}#box{position:fixed;width:160px;height:120px}</style><div id=box></div>",
             "document.documentElement.scrollWidth===160&&document.documentElement.scrollHeight===120"),
            ("<!doctype html><style>html,body{margin:0}#owner{position:relative;width:40px;height:30px;overflow:auto}#middle{width:20px;height:15px;overflow:auto}#box{position:absolute;left:70px;top:80px;width:50px;height:60px}</style><div id=owner><div id=middle><div id=box></div></div></div>",
             "owner.scrollWidth===120&&owner.scrollHeight===140&&middle.scrollWidth===20&&middle.scrollHeight===15&&document.documentElement.scrollWidth===80&&document.documentElement.scrollHeight===60"),
        ] {
            let mut engine=lumen::Engine::new();
            let realm=super::super::install(engine.ctx(),source,128).unwrap();
            realm.set_layout_flusher(Rc::new(|session|session.display_list(80,60,&NoText)
                .map(|_|()).map_err(|error|format!("{error:?}"))));
            let result=engine.eval_value(assertions).unwrap().unwrap_or_else(|error|match engine.describe_throw(error) {
                lumen::Completion::Throw{name,message}=>panic!("positioned scroll area: {name}: {message}"),
                _=>unreachable!("describe_throw returns a throw completion"),
            });
            assert!(matches!(result,Value::Bool(true)),"containing-block routing: {assertions}");
        }
    }

    #[test]
    fn specification_widget_client_geometry_uses_shared_used_display_and_preserves_computed_style() {
        let mut engine=lumen::Engine::new();
        let realm=super::super::install(engine.ctx(),"<!doctype html><style>html,body{margin:0}button,input,span{display:inline;width:100px;height:20px;padding:5px;border:3px solid;box-sizing:content-box}</style><button id=button></button><input id=input type=button value=''><span id=ordinary></span>",128).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(300,100,&NoText)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            for(const element of [button,input]) {
                if(getComputedStyle(element).display!=='inline')throw Error('computed display is authored');
                if(element.clientWidth!==110||element.clientHeight!==30||element.offsetWidth!==116||element.offsetHeight!==36)throw Error('actual widget padding and border boxes');
            }
            if(ordinary.clientWidth!==0||ordinary.clientHeight!==0)throw Error('ordinary inline box stays zero');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|match engine.describe_throw(error) {
            lumen::Completion::Throw{name,message}=>panic!("widget geometry: {name}: {message}"),
            _=>unreachable!("describe_throw returns a throw completion"),
        });
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_float_bfc_border_admission_preserves_signed_margins_actual_height_and_replay() {
        let mut engine=lumen::Engine::new();
        let realm=super::super::install(engine.ctx(),r#"<!doctype html><style>
            html,body{margin:0}main{display:flow-root;width:100px;margin-top:100px}
            #float{float:left;width:50px;height:100px}
            #box{overflow:hidden;width:50px;height:100px;margin-right:1px}
        </style><main id=parent><div id=float></div><div id=box></div></main>"#,128).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(300,600,&NoText)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const parent=document.getElementById('parent'),box=document.getElementById('box'),floating=document.getElementById('float');
            function check(x,y,w,h,reason) {
                const p=parent.getBoundingClientRect(),r=box.getBoundingClientRect();
                if(r.left-p.left!==x||r.top-p.top!==y||r.width!==w||r.height!==h)
                    throw Error(reason+': '+[r.left-p.left,r.top-p.top,r.width,r.height].join(','));
                const again=box.getBoundingClientRect();
                if(again.left!==r.left||again.top!==r.top||again.width!==r.width||again.height!==r.height)throw Error('retained geometry changed');
            }
            check(50,0,50,100,'positive margin may overlap float margin box');
            const margin=box.computedStyleMap().get('margin-right');if(margin.value!==1||margin.unit!=='px')throw Error('used placement changed computed margin');
            parent.style.direction='rtl';floating.style.cssFloat='right';box.style.marginRight='0px';box.style.marginLeft='1px';
            check(0,0,50,100,'RTL margin equation preserves symmetric border exclusion');
            parent.style.direction='ltr';floating.style.cssFloat='left';box.style.marginLeft='0px';
            floating.style.height='50px';box.style.marginRight='0px';box.style.height='50px';

            for(const [margin,width,x,y] of [[-75,50,0,-75],[-75,75,0,-75],[-25,50,50,-25],[-25,75,0,50]]) {
                box.style.marginTop=margin+'px';box.style.width=width+'px';check(x,y,width,50,'actual signed border interval');
            }
            box.style.marginTop='0';box.style.height='auto';box.innerHTML='<div style="height:75px"></div>';
            check(0,50,75,75,'auto height uses real child box at candidate band');
            parent.style.marginTop='120px';check(0,50,75,75,'mutation and retained replay keep source placement');
            box.style.width='50px';box.style.marginLeft='-1px';
            check(50,0,50,75,'negative margin plane may overlap but border plane must avoid float');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|match engine.describe_throw(error) {
            lumen::Completion::Throw{name,message}=>panic!("BFC float admission: {name}: {message}"),
            _=>unreachable!("describe_throw returns a throw completion"),
        });
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn document_point_queries_use_live_layout_pointer_events_and_root_fallback() {
        let mut engine = lumen::Engine::new();
        let realm = super::super::install(engine.ctx(), "<style>body{margin:0}#outer{width:20px;height:20px}#inner{width:5px;height:5px}</style><div id=outer><div id=inner></div></div>", 128).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(30, 30, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"
            if (window.innerWidth !== 30 || window.innerHeight !== 30) throw new Error('live viewport dimensions');
            const outer = document.getElementById('outer');
            const inner = document.getElementById('inner');
            const first = document.elementsFromPoint(2, 2);
            if (first.map(e => e.localName).join(',') !== 'div,div,body,html' || first[0] !== inner || document.elementFromPoint(2, 2) !== inner) throw new Error('paint order');
            outer.style.pointerEvents = 'none';
            if (document.elementsFromPoint(2, 2).includes(inner) || getComputedStyle(inner).pointerEvents !== 'none' || inner.getBoundingClientRect().width !== 5) throw new Error('inherited pointer eligibility');
            inner.style.pointerEvents = 'auto';
            if (document.elementFromPoint(2, 2) !== inner) throw new Error('override');
            if (document.elementsFromPoint(-1, 0).length || document.elementsFromPoint(-Number.MIN_VALUE, 0).length || document.elementFromPoint(31, 2) !== null) throw new Error('outside');
            if (document.elementFromPoint(30, 2) !== document.documentElement) throw new Error('viewport edge fallback');
            if (document.elementFromPoint(29, 29) !== document.documentElement) throw new Error('root fallback');
            let threw = false; try { document.elementsFromPoint(NaN, 0); } catch(e) { threw = e instanceof TypeError; }
            const host = document.createElement('div');
            host.style.cssText = 'width:10px;height:10px';
            document.body.appendChild(host);
            const shadow = host.attachShadow({mode:'closed'});
            const box = document.createElement('div');
            box.style.cssText = 'width:10px;height:10px';
            shadow.appendChild(box);
            const bounds = box.getBoundingClientRect();
            if (document.elementFromPoint(bounds.left + 2, bounds.top + 2) !== host || shadow.elementFromPoint(bounds.left + 2, bounds.top + 2) !== box) throw new Error('shadow retarget');
            threw
        "#).expect("valid point-query script");
        match result {
            Ok(Value::Bool(true)) => (),
            Ok(_) => panic!("point-query guard returned false"),
            Err(error) => {
                let message = engine.ctx().get_member(&error, "message").ok();
                panic!(
                    "point-query script failed: {}",
                    message
                        .and_then(|value| if let Value::Str(text) = value {
                            Some(text.to_string())
                        } else {
                            None
                        })
                        .unwrap_or_default()
                );
            }
        }
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(60, 45, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        match engine.eval_value("window.innerWidth === 60 && window.innerHeight === 45") {
            Ok(Ok(Value::Bool(true))) => (),
            _ => panic!("window dimensions did not follow the resized viewport"),
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
