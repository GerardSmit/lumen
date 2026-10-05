//! Shared CSSOM scroll option conversion and DOM scrolling operations.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsHost, Promise, Slot};
use lumen_bind::{FromArg, Host};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ScrollBehavior {
    #[default]
    Auto,
    Instant,
    Smooth,
}

/// The inherited CSSOM View `ScrollOptions` dictionary together with the
/// optional destination axes from `ScrollToOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScrollToOptions {
    pub behavior: ScrollBehavior,
    pub left: Option<f64>,
    pub top: Option<f64>,
}

impl<'a> FromArg<'a, JsHost> for ScrollToOptions {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        _at: Slot,
    ) -> Result<Self, Value> {
        if matches!(value, Value::Undefined | Value::Null) {
            return Ok(Self::default());
        }
        <JsHost as Host>::with_ctx(cx, |ctx| {
            if !matches!(value, Value::Obj(_)) {
                return Err(ctx.make_error("TypeError", "ScrollToOptions requires an object"));
            }
            // Inherited ScrollOptions members are converted before the
            // operation-specific ScrollToOptions members.
            let behavior = behavior_from_dictionary(ctx, value)?;
            let left = optional_scroll_coordinate(ctx, value, "left")?;
            let top = optional_scroll_coordinate(ctx, value, "top")?;
            Ok(Self {
                behavior,
                left,
                top,
            })
        })
    }
}

fn optional_scroll_coordinate(
    ctx: &mut Ctx,
    options: &Value,
    name: &str,
) -> Result<Option<f64>, Value> {
    let value = ctx.member_get(options, name)?;
    if matches!(value, Value::Undefined) {
        Ok(None)
    } else {
        ctx.coerce_number(&value).map(Some)
    }
}

/// ScrollOptions is the common inherited dictionary, converted before the
/// operation-specific members. Only undefined selects its default.
pub(crate) fn behavior_from_dictionary(
    ctx: &mut Ctx,
    options: &Value,
) -> Result<ScrollBehavior, Value> {
    let value = ctx.member_get(options, "behavior")?;
    if matches!(value, Value::Undefined) {
        return Ok(ScrollBehavior::Auto);
    }
    let value = ctx.coerce_string(&value)?;
    match value.as_ref() {
        "auto" => Ok(ScrollBehavior::Auto),
        "instant" => Ok(ScrollBehavior::Instant),
        "smooth" => Ok(ScrollBehavior::Smooth),
        _ => Err(ctx.make_error("TypeError", "Invalid scroll behavior")),
    }
}

pub(crate) use lumen_common::scroll::Alignment as ScrollAlignment;

fn alignment_from_dictionary(
    ctx: &mut Ctx,
    options: &Value,
    member: &str,
    default: ScrollAlignment,
) -> Result<ScrollAlignment, Value> {
    let value = ctx.member_get(options, member)?;
    if matches!(value, Value::Undefined) {
        return Ok(default);
    }
    let value = ctx.coerce_string(&value)?;
    match value.as_ref() {
        "start" => Ok(ScrollAlignment::Start),
        "center" => Ok(ScrollAlignment::Center),
        "end" => Ok(ScrollAlignment::End),
        "nearest" => Ok(ScrollAlignment::Nearest),
        _ => Err(ctx.make_error("TypeError", "Invalid scroll alignment")),
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IntoViewOptions {
    pub behavior: ScrollBehavior,
    pub block: ScrollAlignment,
    pub inline: ScrollAlignment,
    pub nearest_container: bool,
}

impl Default for IntoViewOptions {
    fn default() -> Self {
        Self {
            behavior: ScrollBehavior::Auto,
            block: ScrollAlignment::Start,
            inline: ScrollAlignment::Nearest,
            nearest_container: false,
        }
    }
}

impl<'a> FromArg<'a, JsHost> for IntoViewOptions {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        at: Slot,
    ) -> Result<Self, Value> {
        if matches!(value, Value::Undefined | Value::Null) {
            return Ok(Self::default());
        }
        if !matches!(value, Value::Obj(_)) {
            let align_to_top = <JsHost as Host>::to_bool(cx, value, at)?;
            return Ok(Self {
                block: if align_to_top {
                    ScrollAlignment::Start
                } else {
                    ScrollAlignment::End
                },
                ..Self::default()
            });
        }
        <JsHost as Host>::with_ctx(cx, |ctx| {
            let behavior = behavior_from_dictionary(ctx, value)?;
            let block = alignment_from_dictionary(ctx, value, "block", ScrollAlignment::Start)?;
            let container = ctx.member_get(value, "container")?;
            let nearest_container = if matches!(container, Value::Undefined) {
                false
            } else {
                match ctx.coerce_string(&container)?.as_ref() {
                    "all" => false,
                    "nearest" => true,
                    _ => return Err(ctx.make_error("TypeError", "Invalid scroll container")),
                }
            };
            let inline = alignment_from_dictionary(ctx, value, "inline", ScrollAlignment::Nearest)?;
            Ok(Self {
                behavior,
                block,
                inline,
                nearest_container,
            })
        })
    }
}

pub(crate) fn position(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<(f64, f64)> {
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok((0.0, 0.0));
    }
    realm.flush_layout()?;
    let (x, y) = realm.session.borrow().scroll_offset(node);
    Ok((f64::from(x), f64::from(y)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ElementScrollPurpose {
    Getter,
    Setter,
    Method,
}

fn html_body(document: &lumen_html::Document) -> Option<NodeId> {
    let root = selector::document_element(document)?;
    if !matches!(
        document.kind(root),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if lumen_html::svg::local_name(name).eq_ignore_ascii_case("html")
    ) {
        return None;
    }
    let mut child = document.first_child(root).ok().flatten();
    while let Some(node) = child {
        if matches!(
            document.kind(node),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name).eq_ignore_ascii_case("body")
        ) {
            return Some(node);
        }
        child = document.next_sibling(node).ok().flatten();
    }
    None
}

fn potentially_scrollable_axes(
    realm: &Rc<DomRealm>,
    body: NodeId,
    treat_parent_clip_as_hidden: bool,
) -> OpResult<(bool, bool)> {
    realm.flush_layout()?;
    let parent = {
        let session = realm.session.borrow();
        if session.layout_rect(body).is_none() {
            return Ok((false, false));
        }
        session.document().parent(body).map_err(dom_error)?
    };
    let Some(parent) = parent.filter(|node| {
        matches!(
            realm.session.borrow().document().kind(*node),
            Ok(NodeKind::Element { .. })
        )
    }) else {
        return Ok((false, false));
    };
    let (body_style, parent_style) = {
        let mut session = realm.session.borrow_mut();
        let body_style = session.computed_style(body).map_err(|error| {
            OpError::new(
                "InvalidStateError",
                format!("body scroll style failed: {error:?}"),
            )
        })?;
        let parent_style = session.computed_style(parent).map_err(|error| {
            OpError::new(
                "InvalidStateError",
                format!("body parent scroll style failed: {error:?}"),
            )
        })?;
        (body_style, parent_style)
    };
    let body_axis = |overflow| {
        !matches!(
            overflow,
            lumen_html::css::Overflow::Visible | lumen_html::css::Overflow::Clip
        )
    };
    let parent_axis = |overflow| {
        overflow != lumen_html::css::Overflow::Visible
            && (treat_parent_clip_as_hidden || overflow != lumen_html::css::Overflow::Clip)
    };
    Ok((
        body_axis(body_style.overflow_x) && parent_axis(parent_style.overflow_x),
        body_axis(body_style.overflow_y) && parent_axis(parent_style.overflow_y),
    ))
}

/// Return the node whose scroll offset an Element API operates on. The root
/// viewport is represented by the document node; a missing target means the
/// CSSOM operation is inert for this element/document state.
pub(crate) fn element_scroll_target(
    realm: &Rc<DomRealm>,
    node: NodeId,
    purpose: ElementScrollPurpose,
) -> OpResult<Option<NodeId>> {
    if !realm.has_browsing_context
        || !realm
            .browsing_context()
            .is_some_and(|context| browsing_context::is_active_document(&context, realm))
    {
        return Ok(None);
    }

    let (mode, root_element, body) = {
        let session = realm.session.borrow();
        let document = session.document();
        (
            document.document_mode(),
            selector::document_element(document),
            html_body(document),
        )
    };
    let document_root = realm.session.borrow().document().root();
    if root_element == Some(node) {
        return Ok((mode != lumen_html::DocumentMode::Quirks).then_some(document_root));
    }

    if mode == lumen_html::DocumentMode::Quirks && body == Some(node) {
        // The legacy body-to-viewport rule differs between the individual
        // offset attributes and scrolling methods in CSSOM View.
        let (x, y) = potentially_scrollable_axes(realm, node, false)?;
        let viewport = match purpose {
            ElementScrollPurpose::Getter | ElementScrollPurpose::Setter => !x || !y,
            ElementScrollPurpose::Method => !x && !y,
        };
        if viewport {
            return Ok(Some(document_root));
        }
    }

    realm.flush_layout()?;
    let session = realm.session.borrow();
    if session.layout_rect(node).is_none() {
        return Ok(None);
    }
    if purpose != ElementScrollPurpose::Getter && session.scroll_extent(node).is_none() {
        return Ok(None);
    }
    Ok(Some(node))
}

/// The document's `scrollingElement` follows quirks mode's body rule and is
/// recalculated from the current computed overflow styles on each access.
pub(crate) fn document_scrolling_element(realm: &Rc<DomRealm>) -> OpResult<Option<NodeId>> {
    let (mode, root, body) = {
        let session = realm.session.borrow();
        let document = session.document();
        (
            document.document_mode(),
            selector::document_element(document),
            html_body(document),
        )
    };
    if mode == lumen_html::DocumentMode::Quirks {
        let Some(body) = body else {
            return Ok(None);
        };
        let (x, y) = potentially_scrollable_axes(realm, body, true)?;
        return Ok((!x && !y).then_some(body));
    }
    Ok(root)
}

#[derive(Default)]
struct ScrollEvents {
    pending: HashSet<NodeId>,
}

fn queue_scroll_events(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let queue = match RealmServices::<RefCell<ScrollEvents>>::current(ctx) {
        Some(queue) => queue,
        None => {
            let queue = Rc::new(RefCell::new(ScrollEvents::default()));
            RealmServices::replace_shared_current(ctx, queue.clone());
            queue
        }
    };
    {
        let mut state = queue.borrow_mut();
        if state.pending.contains(&node) {
            return Ok(());
        }
        state.pending.try_reserve(1).map_err(|_| {
            OpError::new("QuotaExceededError", "scroll event queue allocation failed")
        })?;
        state.pending.insert(node);
    }
    let target = realm.wrap(ctx, node);
    let realm = realm.clone();
    let pending = queue.clone();
    if let Err(error) = scheduling::queue_task(ctx, move |ctx| {
        let _target = target;
        pending.borrow_mut().pending.remove(&node);
        let root = realm.session.borrow().document().root();
        if realm.session.borrow().document().kind(node).is_err() {
            return Ok(());
        }
        realm.dispatch_user_agent(ctx, node, "scroll", node == root, false, &[])?;
        if realm.session.borrow().document().kind(node).is_ok() {
            realm.dispatch_user_agent(ctx, node, "scrollend", node == root, false, &[])?;
        }
        Ok(())
    }) {
        queue.borrow_mut().pending.remove(&node);
        return Err(error);
    }
    Ok(())
}

/// Apply a real session scroll and queue its notifications in the owning
/// document realm. Values are clamped before narrowing to renderer coordinates.
pub(crate) fn set_offset(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    x: f64,
    y: f64,
    behavior: ScrollBehavior,
) -> Promise<()> {
    Promise::ready(apply_offset(ctx, realm, node, x, y, behavior))
}

pub(crate) fn apply_offset(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    x: f64,
    y: f64,
    behavior: ScrollBehavior,
) -> OpResult<()> {
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok(());
    }
    realm.flush_layout()?;
    let changed = {
        let mut session = realm.session.borrow_mut();
        let root = session.document().root();
        let Some((max_x, max_y)) = session
            .scroll_extent(node)
            .or_else(|| (node == root).then_some((0.0, 0.0)))
        else {
            return Ok(());
        };
        let x = if x.is_finite() { x } else { 0.0 };
        let y = if y.is_finite() { y } else { 0.0 };
        let x = x.clamp(0.0, f64::from(max_x.max(0.0))) as f32;
        let y = y.clamp(0.0, f64::from(max_y.max(0.0))) as f32;
        if session.scroll_offset(node) == (x, y) {
            return Ok(());
        }
        if behavior == ScrollBehavior::Smooth {
            return Err(OpError::new(
                "NotSupportedError",
                "smooth scrolling is not implemented",
            ));
        }
        session.set_scroll_offset(node, x, y).map_err(|error| {
            OpError::new("InvalidStateError", format!("scroll failed: {error:?}"))
        })?
    };
    if changed {
        if let Some(context) = realm.browsing_context() {
            let handle = browsing_context::context_realm_handle(&context);
            ctx.with_host_realm(&handle, |ctx| queue_scroll_events(ctx, realm, node))
                .map_err(browsing_context::host_realm_error)??;
        } else {
            queue_scroll_events(ctx, realm, node)?;
        }
    }
    Ok(())
}

/// Dispatch the trusted pixel-mode wheel event and apply its uncanceled
/// default scrolling action through real ancestor scrollports. Each axis can
/// chain independently when the nearer scrollport has reached its extent.
/// `overscroll-behavior` is not represented by the current style model, so the
/// implemented profile uses the default chaining behavior.
pub(crate) fn dispatch_wheel(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    client_x: f64,
    client_y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: ui_events::UserAgentModifiers,
) -> OpResult<()> {
    if !client_x.is_finite()
        || !client_y.is_finite()
        || !delta_x.is_finite()
        || !delta_y.is_finite()
    {
        return Err(OpError::type_error(
            "wheel coordinates and deltas must be finite",
        ));
    }
    let event = ui_events::user_agent_wheel_event(
        ctx,
        realm
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
            .unwrap_or(Value::Null),
        client_x,
        client_y,
        delta_x,
        delta_y,
        modifiers,
    )?;
    let event = lumen::embed::JsObject::from_value(event)
        .ok_or_else(|| OpError::type_error("user-agent WheelEvent is not an object"))?;
    let receiver = realm.wrap(ctx, target);
    if events::dispatch_user_agent_event(ctx, lumen_bind::This(receiver), event)? {
        apply_wheel_default_action(ctx, realm, target, delta_x, delta_y)?;
    }
    Ok(())
}

fn apply_wheel_default_action(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    delta_x: f64,
    delta_y: f64,
) -> OpResult<()> {
    if delta_x == 0.0 && delta_y == 0.0 {
        return Ok(());
    }
    realm.flush_layout()?;
    let (root, root_element) = {
        let session = realm.session.borrow();
        (
            session.document().root(),
            selector::document_element(session.document()),
        )
    };
    let mut remaining_x = delta_x;
    let mut remaining_y = delta_y;
    let mut current = Some(target);
    while let Some(node) = current {
        let parent = realm
            .session
            .borrow()
            .document()
            .composed_parent(node)
            .map_err(dom_error)?;
        let style_node = if node == root {
            root_element.unwrap_or(node)
        } else {
            node
        };
        let (overflow_x, overflow_y, extent, offset) = {
            let mut session = realm.session.borrow_mut();
            let style = session.computed_style(style_node).map_err(|error| {
                OpError::new(
                    "InvalidStateError",
                    format!("wheel scroll style failed: {error:?}"),
                )
            })?;
            (
                style.overflow_x,
                style.overflow_y,
                session.scroll_extent(node),
                session.scroll_offset(node),
            )
        };
        let Some((max_x, max_y)) = extent else {
            if node == root {
                break;
            }
            current = parent;
            continue;
        };
        let user_scrollable = |overflow| {
            matches!(
                overflow,
                lumen_html::css::Overflow::Auto | lumen_html::css::Overflow::Scroll
            )
        };
        let x_scrollable = user_scrollable(overflow_x)
            || (node == root && overflow_x == lumen_html::css::Overflow::Visible);
        let y_scrollable = user_scrollable(overflow_y)
            || (node == root && overflow_y == lumen_html::css::Overflow::Visible);
        let mut next_x = f64::from(offset.0);
        let mut next_y = f64::from(offset.1);
        if x_scrollable && remaining_x != 0.0 {
            let limit = f64::from(max_x.max(0.0));
            let candidate = (next_x + remaining_x).clamp(0.0, limit);
            remaining_x -= candidate - next_x;
            next_x = candidate;
        }
        if y_scrollable && remaining_y != 0.0 {
            let limit = f64::from(max_y.max(0.0));
            let candidate = (next_y + remaining_y).clamp(0.0, limit);
            remaining_y -= candidate - next_y;
            next_y = candidate;
        }
        if next_x != f64::from(offset.0) || next_y != f64::from(offset.1) {
            apply_offset(ctx, realm, node, next_x, next_y, ScrollBehavior::Auto)?;
        }
        if (remaining_x == 0.0 && remaining_y == 0.0) || node == root {
            break;
        }
        current = parent;
    }
    Ok(())
}

/// Scroll each real ancestor scrollport using the target's current transformed
/// border box. Re-read geometry after each movement; retained coordinates of
/// outer scrollers can change when an inner scroller moves.
pub(crate) fn into_view(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    options: IntoViewOptions,
) -> Promise<()> {
    Promise::ready(into_view_inner(ctx, realm, target, options))
}

fn into_view_inner(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    options: IntoViewOptions,
) -> OpResult<()> {
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok(());
    }
    {
        let session = realm.session.borrow();
        let document = session.document();
        if document.root_node(target, true).map_err(dom_error)? != document.root() {
            return Ok(());
        }
    }
    realm.flush_layout()?;
    let mut ancestor = Some(target);
    while let Some(node) = ancestor {
        // A viewport-fixed subtree may scroll its own local containers, but
        // scrolling its document cannot bring it closer to the viewport.
        let fixed = realm.session.borrow().is_viewport_fixed(node);
        let next = realm
            .session
            .borrow()
            .document()
            .composed_parent(node)
            .map_err(dom_error)?;
        // The target's own scrollport does not scroll its border box.
        if node != target {
            realm.flush_layout()?;
            let geometry = {
                let mut session = realm.session.borrow_mut();
                match (
                    session.bounding_client_rect(target),
                    session.scrollport(node),
                ) {
                    (Some(bounds), Some(port)) => {
                        let style_node = if node == session.document().root() {
                            selector::document_element(session.document()).unwrap_or(node)
                        } else {
                            node
                        };
                        let style = session.computed_style(style_node).map_err(|error| {
                            OpError::new(
                                "InvalidStateError",
                                format!("scroll style failed: {error:?}"),
                            )
                        })?;
                        Some((
                            bounds,
                            port,
                            session.scroll_offset(node),
                            style.writing_mode,
                            style.direction,
                        ))
                    }
                    _ => None,
                }
            };
            if let Some((bounds, port, (x, y), writing_mode, direction)) = geometry {
                use lumen_common::scroll::alignment_delta;
                use lumen_html::css::{Direction, WritingMode};
                let rtl = direction == Direction::Rtl;
                let (x_align, y_align, x_reverse, y_reverse) = match writing_mode {
                    WritingMode::HorizontalTb => (options.inline, options.block, rtl, false),
                    WritingMode::VerticalRl => (options.block, options.inline, true, rtl),
                    WritingMode::VerticalLr => (options.block, options.inline, false, rtl),
                };
                let dx = alignment_delta(
                    f64::from(bounds.x),
                    f64::from(bounds.x + bounds.width),
                    f64::from(port.x),
                    f64::from(port.x + port.width),
                    x_align,
                    x_reverse,
                );
                let dy = alignment_delta(
                    f64::from(bounds.y),
                    f64::from(bounds.y + bounds.height),
                    f64::from(port.y),
                    f64::from(port.y + port.height),
                    y_align,
                    y_reverse,
                );
                apply_offset(
                    ctx,
                    realm,
                    node,
                    f64::from(x) + dx,
                    f64::from(y) + dy,
                    options.behavior,
                )?;
                if options.nearest_container {
                    break;
                }
            }
        }
        if fixed {
            break;
        }
        ancestor = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    fn setup(source: &str) -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), source, 32).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        (engine, realm)
    }

    fn script(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid script") {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "message")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(message) => Some(message.to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "script threw".into());
                panic!("scrolling script failed: {message}");
            }
        }
    }

    #[test]
    fn wheel_event_uses_trusted_pixel_mode_and_chains_uncanceled_axes() {
        let (mut engine, realm) = setup(
            "<style>html,body{margin:0}#outer{overflow:auto;width:60px;height:30px}#spacer{height:50px}#inner{overflow:auto;width:30px;height:20px}#content{height:50px;width:50px}</style><div id='outer'><div id='spacer'></div><div id='inner'><div id='content'></div></div></div>",
        );
        script(
            &mut engine,
            "globalThis.inner=document.getElementById('inner'); globalThis.outer=document.getElementById('outer'); globalThis.wheelLog=[]; inner.addEventListener('wheel', event => wheelLog.push([event.isTrusted,event.deltaMode,event.deltaY,event.ctrlKey])); true",
        );
        let element = script(&mut engine, "inner");
        let node = engine
            .ctx()
            .with_instance::<DomElement, _>(&element, |element| element.base.id)
            .ok()
            .expect("inner is a native Element");
        assert!(dispatch_wheel(
            engine.ctx(),
            &realm,
            node,
            5.0,
            5.0,
            0.0,
            10.0,
            ui_events::UserAgentModifiers {
                ctrl: true,
                ..Default::default()
            },
        )
        .is_ok());
        assert!(matches!(
            script(
                &mut engine,
                "inner.scrollTop===10 && wheelLog[0][0]===true && wheelLog[0][1]===WheelEvent.DOM_DELTA_PIXEL && wheelLog[0][2]===10 && wheelLog[0][3]===true",
            ),
            Value::Bool(true)
        ));

        script(&mut engine, "inner.addEventListener('wheel', event => event.preventDefault(), {once:true}); inner.scrollTop=10; true");
        assert!(dispatch_wheel(
            engine.ctx(),
            &realm,
            node,
            5.0,
            5.0,
            0.0,
            7.0,
            ui_events::UserAgentModifiers::default(),
        )
        .is_ok());
        assert!(matches!(
            script(&mut engine, "inner.scrollTop===10 && outer.scrollTop===0"),
            Value::Bool(true)
        ));

        script(&mut engine, "inner.scrollTop=30; true");
        assert!(dispatch_wheel(
            engine.ctx(),
            &realm,
            node,
            5.0,
            5.0,
            0.0,
            8.0,
            ui_events::UserAgentModifiers::default(),
        )
        .is_ok());
        assert!(matches!(
            script(&mut engine, "inner.scrollTop===30 && outer.scrollTop===8"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn into_view_scrolls_real_nested_ports_and_converts_options_before_layout() {
        let (mut engine, _realm) = setup("<style>html,body{margin:0}#outer{overflow:auto;width:30px;height:30px}#inner{overflow:auto;width:20px;height:20px}#spacer{height:40px}#target{width:10px;height:10px}</style><div id='outer'><div id='spacer'></div><div id='inner'><div style='height:40px'></div><div id='target'></div></div></div>");
        let result = script(
            &mut engine,
            r#"
            const outer = document.getElementById('outer'), inner = document.getElementById('inner');
            const target = document.getElementById('target');
            const order = [];
            const promise = target.scrollIntoView({
                get behavior() { order.push('behavior'); return 'instant'; },
                get block() { order.push('block'); return 'start'; },
                get container() { order.push('container'); return 'nearest'; },
                get inline() { order.push('inline'); return 'nearest'; }
            });
            if (!(promise instanceof Promise) || order.join(',') !== 'behavior,block,container,inline')
                throw new Error('option order or return brand');
            if (inner.scrollTop !== 30 || outer.scrollTop !== 0)
                throw new Error('nearest must move only the inner scrollport: '+inner.scrollTop+','+outer.scrollTop);
            target.scrollIntoView({behavior:'instant', block:'end', inline:'nearest'});
            const bounds = target.getBoundingClientRect(), port = outer.getBoundingClientRect();
            if (outer.scrollTop !== 30 || bounds.bottom > port.bottom || bounds.top < port.top)
                throw new Error('all ancestor scrolling must reveal the target: '+outer.scrollTop);
            let rejected = false;
            try { target.scrollIntoView({block:'invalid'}); } catch(error) { rejected = error.name === 'TypeError'; }
            if (!rejected) throw new Error('invalid enum accepted');
            const detached = document.createElement('div');
            if (!(detached.scrollIntoView(false) instanceof Promise)) throw new Error('detached return');
            true
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn scroll_events_are_trusted_queued_coalesced_and_root_bubbles_to_window() {
        let (mut engine, _realm) = setup("<style>html,body{margin:0}body{height:300px}</style>");
        script(
            &mut engine,
            r#"
            globalThis.events = [];
            window.addEventListener('scroll', event => events.push(event.type+':'+event.isTrusted));
            window.addEventListener('scrollend', event => events.push(event.type+':'+event.isTrusted));
            window.scrollTo(0, 10);
            window.scrollTo(0, 20);
            if (events.length !== 0) throw new Error('synchronous scroll event');
        "#,
        );
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        let result = script(
            &mut engine,
            "events.join(',') === 'scroll:true,scrollend:true'",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn document_scrolling_element_and_element_offsets_follow_standard_and_quirks_mapping() {
        let (mut standard, _) = setup(
            "<!doctype html><style>html,body{margin:0}body{margin-top:100px;height:40px;width:40px;overflow:auto}#content{height:300px;width:300px}</style><div id=content></div>",
        );
        let result = script(
            &mut standard,
            r#"
            const root = document.documentElement, body = document.body;
            if (document.scrollingElement !== root) throw new Error('standard scrollingElement is not root');
            root.scrollTop = 23;
            if (root.scrollTop !== 23 || window.scrollY !== 23) throw new Error('standard root offset did not map to viewport');
            window.scrollTo(0, 0);
            body.scrollTop = 11;
            if (body.scrollTop !== 11 || window.scrollY !== 0) throw new Error('standard body offset did not stay on body');
            body.scrollTo(0, 15);
            body.scrollTop === 15 && window.scrollY === 0
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));

        let (mut quirks, _) = setup(
            "<style>html,body{margin:0}body{height:300px}</style><div id=content style='height:300px;width:300px'></div>",
        );
        let result = script(
            &mut quirks,
            r#"
            const root = document.documentElement, body = document.body;
            if (document.compatMode !== 'BackCompat' || document.scrollingElement !== body)
                throw new Error('initial quirks scrollingElement is not body');
            body.scrollTop = 17;
            if (body.scrollTop !== 17 || window.scrollY !== 17) throw new Error('quirks body offset did not map to viewport');
            root.scrollTop = 5;
            if (root.scrollTop !== 0 || window.scrollY !== 17) throw new Error('quirks root offset was not inert');
            root.style.overflow = 'hidden';
            body.style.cssText = 'height:40px;width:40px;overflow:auto';
            if (document.scrollingElement !== null) throw new Error('scrollingElement did not update for potentially-scrollable body');
            window.scrollTo(0, 0);
            body.scrollTop = 11;
            if (body.scrollTop !== 11 || window.scrollY !== 0) throw new Error('dynamic quirks body offset did not map to body');
            body.scrollTo(0, 15);
            body.scrollTop === 15 && window.scrollY === 0
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn into_view_stops_at_viewport_fixed_roots_but_scrolls_their_local_containers() {
        let (mut engine, _realm) = setup("<style>html,body{margin:0}body{height:400px}#fixed{position:fixed;top:100px;left:0;overflow:auto;width:20px;height:20px}#target{height:10px}#transformed{transform:translateY(100px);height:150px}#inside{position:fixed;top:100px;width:10px;height:10px}</style><div id='fixed'><div style='height:40px'></div><div id='target'></div></div><div id='transformed'><div id='inside'></div></div>");
        let result = script(
            &mut engine,
            r#"
            const fixed = document.getElementById('fixed'), target = document.getElementById('target');
            target.scrollIntoView({behavior:'instant', block:'start'});
            if (fixed.scrollTop !== 30 || window.scrollY !== 0)
                throw new Error('viewport-fixed subtree must scroll locally: '+fixed.scrollTop+','+window.scrollY);
            fixed.scrollIntoView({behavior:'instant'});
            if (window.scrollY !== 0) throw new Error('fixed element scrolled viewport');
            document.getElementById('inside').scrollIntoView({behavior:'instant'});
            if (window.scrollY === 0) throw new Error('transformed fixed containing block did not scroll viewport');
            true
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
