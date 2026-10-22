//! `CaretPosition`, `Document.caretPositionFromPoint()` and `Document.caretRangeFromPoint()`.
use super::*;
use lumen_html::ranges::Boundary;

#[lumen_bind::class(name = "CaretPosition", hint(js(webidl)))]
pub struct DomCaretPosition {
    realm: Rc<DomRealm>,
    node: NodeId,
    offset: u32,
}

#[lumen_bind::methods]
impl DomCaretPosition {
    #[getter]
    fn offset_node(&self, ctx: &mut Ctx) -> Value {
        self.realm.wrap(ctx, self.node)
    }

    #[getter]
    fn offset(&self) -> u32 {
        self.offset
    }

    fn get_client_rect(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let point = Boundary { container: self.node, offset: self.offset as usize };
        let rect = geometry::collapsed_range_rect(ctx, &self.realm, point)?;
        Ok(rect.map_or(Value::Null, |rect| geometry::rect_value(ctx, rect)))
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<DomCaretPosition>();
    crate::install_interface(ctx, &global, "CaretPosition", constructor).map_err(OpError::thrown)
}

fn text_or_element_position(ctx: &mut Ctx, realm: &Rc<DomRealm>, x: f64, y: f64) -> OpResult<Option<(NodeId, usize)>> {
    if !x.is_finite() || !y.is_finite() {
        return Err(OpError::type_error("Point coordinates must be finite"));
    }
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok(None);
    }
    animations::flush_css_transitions_for_property(ctx, realm, None, Some("width"))?;
    realm.session.borrow_mut().set_selectable_text(true);
    realm.flush_layout()?;
    let session = realm.session.borrow();
    let Some((width, height)) = session.viewport_size() else { return Ok(None) };
    if x < 0.0 || y < 0.0 || x > f64::from(width) || y > f64::from(height) {
        return Ok(None);
    }
    if let Some(position) = session.text_position_from_point(x as f32, y as f32) {
        return Ok(Some(position));
    }
    drop(session);
    let Some(element) = geometry::element_from_point_in_tree(realm, None, x, y)? else { return Ok(None) };
    let session = realm.session.borrow();
    let document = session.document();
    if matches!(lumen_html::forms::html_element_local_name(document, element), Some("audio" | "video")) {
        if let Some(parent) = document.parent(element).map_err(dom_error)? {
            let index = range::child_index(document, element)?;
            return Ok(Some((parent, index)));
        }
    }
    Ok(Some((element, 0)))
}

pub(crate) fn caret_position_from_point(ctx: &mut Ctx, realm: &Rc<DomRealm>, x: f64, y: f64) -> OpResult<Value> {
    Ok(match text_or_element_position(ctx, realm, x, y)? {
        Some((node, offset)) => ctx.new_instance(DomCaretPosition { realm: realm.clone(), node, offset: offset as u32 }),
        None => Value::Null,
    })
}

pub(crate) fn caret_range_from_point(ctx: &mut Ctx, realm: &Rc<DomRealm>, x: f64, y: f64) -> OpResult<Value> {
    match text_or_element_position(ctx, realm, x, y)? {
        Some((container, offset)) => range::collapsed_range_at(ctx, realm, Boundary { container, offset }),
        None => Ok(Value::Null),
    }
}
