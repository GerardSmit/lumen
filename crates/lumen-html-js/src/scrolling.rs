//! Shared CSSOM scroll option conversion and DOM scrolling operations.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsHost, Promise, Slot};
use lumen_bind::{FromArg, Host};
use std::collections::HashMap;

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

pub(crate) fn html_body(document: &lumen_html::Document) -> Option<NodeId> {
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
    potentially_scrollable_axes_in_session(&mut realm.session.borrow_mut(), body, treat_parent_clip_as_hidden)
}

pub(crate) fn potentially_scrollable_axes_in_session(
    session: &mut lumen_html::session::RenderSession,
    body: NodeId,
    treat_parent_clip_as_hidden: bool,
) -> OpResult<(bool, bool)> {
    if session.layout_rect(body).is_none() {
        return Ok((false, false));
    }
    let parent = session.document().parent(body).map_err(dom_error)?;
    let Some(parent) = parent.filter(|node| {
        matches!(
            session.document().kind(*node),
            Ok(NodeKind::Element { .. })
        )
    }) else {
        return Ok((false, false));
    };
    let (body_style, parent_style) = {
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
    pending: Vec<ScrollNotification>,
    spare: Vec<ScrollNotification>,
    next_order: usize,
}

struct ScrollNotification {
    node: NodeId,
    owner: std::rc::Weak<DomRealm>,
    _retention: NodeRetention,
    moved: Option<usize>,
    complete: Option<usize>,
}

pub(crate) fn scroll_events_pending(ctx: &mut Ctx) -> bool {
    RealmServices::<RefCell<ScrollEvents>>::current(ctx)
        .is_some_and(|queue| !queue.borrow().pending.is_empty())
}

fn queue_scroll_events(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let complete = RealmServices::<RefCell<SmoothScrolls>>::current(ctx).is_none_or(|state| {
        let mut state = state.borrow_mut();
        if let Some(active) = state.active.get_mut(&node) { active.moved = true; false } else { true }
    });
    queue_scroll_notification(ctx, realm, node, true, complete)
}

fn queue_scroll_notification(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, moved: bool, complete: bool) -> OpResult<()> {
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
        let index = match state.pending.iter().position(|notification| notification.node == node) {
            Some(index) => index,
            None => {
                state.pending.try_reserve(1).map_err(|_| {
                    OpError::new("QuotaExceededError", "scroll event queue allocation failed")
                })?;
                state.pending.push(ScrollNotification { node, owner: Rc::downgrade(realm),
                    _retention: NodeRetention::new(realm, node), moved: None, complete: None });
                state.pending.len() - 1
            }
        };
        if moved && state.pending[index].moved.is_none() {
            state.pending[index].moved = Some(state.next_order);
            state.next_order += 1;
        }
        if complete && state.pending[index].complete.is_none() {
            state.pending[index].complete = Some(state.next_order);
            state.next_order += 1;
        }
    }
    Ok(())
}

/// CSSOM View's document scroll phase runs before animation callbacks. Take
/// one ordered snapshot so author-triggered scrolls remain for the next phase,
/// and deliver every scroll target before any completed target.
pub(crate) fn run_scroll_steps(ctx: &mut Ctx) -> OpResult<()> {
    let Some(queue) = RealmServices::<RefCell<ScrollEvents>>::current(ctx) else { return Ok(()); };
    let mut snapshot = {
        let mut queue = queue.borrow_mut();
        queue.next_order = 0;
        let pending = core::mem::take(&mut queue.pending);
        queue.pending = core::mem::take(&mut queue.spare);
        pending
    };
    let mut failure = None;
    for complete in [false, true] {
        snapshot.sort_unstable_by_key(|notification| if complete { notification.complete } else { notification.moved });
        for notification in &snapshot {
            if !RealmServices::<RefCell<ScrollEvents>>::current(ctx).is_some_and(|current| Rc::ptr_eq(&current, &queue)) {
                return failure.map_or(Ok(()), Err);
            }
            if if complete { notification.complete.is_none() } else { notification.moved.is_none() } { continue; }
            let Some(source) = notification.owner.upgrade().filter(|owner| !owner.has_browsing_context
                || owner.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, owner))) else { continue; };
            let (owner, node) = source.resolve_adopted_node(notification.node);
            let root = owner.session.borrow().document().root();
            if owner.session.borrow().document().kind(node).is_err() { continue; }
            let dispatch = |ctx: &mut Ctx| {
                if complete && smooth_scroll_active(ctx, node) { return Ok(()); }
                let result = owner.dispatch_user_agent(ctx, node,
                    if complete { "scrollend" } else { "scroll" }, node == root, false, &[]).map(|_| ());
                ctx.drain_microtasks_for_host();
                result
            };
            let Some(target_realm) = owner.relevant_host_realm(ctx) else { continue; };
            let result = {
                ctx.with_host_realm(&target_realm, dispatch)
                    .map_err(browsing_context::host_realm_error).and_then(|result| result)
            };
            if let Err(error) = result {
                if failure.is_none() { failure = Some(error); }
            }
        }
    }
    snapshot.clear();
    let mut state = queue.borrow_mut();
    if state.pending.is_empty() { state.pending = snapshot; } else { state.spare = snapshot; }
    failure.map_or(Ok(()), Err)
}

// CSSOM View permits a user-agent-defined interpolation and duration. Keep
// only active scrolls and reuse the frame snapshot allocation; no DOM node or
// native wrapper is rooted by an animation record.
const SMOOTH_SCROLL_DURATION_MS: f64 = 240.0;

#[derive(Clone)]
struct SmoothScroll {
    owner: std::rc::Weak<DomRealm>,
    from: (f32, f32),
    to: (f32, f32),
    start_ms: f64,
    end_ms: f64,
    generation: u64,
    moved: bool,
}

#[derive(Default)]
struct SmoothScrolls {
    active: HashMap<NodeId, SmoothScroll>,
    snapshot: Vec<(NodeId, SmoothScroll)>,
    updates: Vec<lumen_html::session::ScrollUpdate>,
    generation: u64,
}

fn smooth_scroll_active(ctx: &mut Ctx, node: NodeId) -> bool {
    RealmServices::<RefCell<SmoothScrolls>>::current(ctx)
        .is_some_and(|state| state.borrow().active.contains_key(&node))
}

pub(crate) fn smooth_scroll_pending(ctx: &mut Ctx) -> bool {
    RealmServices::<RefCell<SmoothScrolls>>::current(ctx)
        .is_some_and(|state| !state.borrow().active.is_empty())
}

pub(crate) fn rebind_scroll_document(ctx: &mut Ctx) {
    if let Some(state) = RealmServices::<RefCell<SmoothScrolls>>::current(ctx) {
        let mut state = state.borrow_mut();
        state.active = HashMap::new();
        state.snapshot = Vec::new(); state.updates = Vec::new();
    }
    if let Some(events) = RealmServices::<RefCell<ScrollEvents>>::current(ctx) {
        events.borrow_mut().pending.clear();
        RealmServices::replace_current(ctx, RefCell::new(ScrollEvents::default()));
    }
}

fn abort_smooth_scroll(ctx: &mut Ctx, node: NodeId) -> Option<SmoothScroll> {
    RealmServices::<RefCell<SmoothScrolls>>::current(ctx)
        .and_then(|state| {
            let record = state.borrow_mut().active.remove(&node);
            record
        })
}

fn release_idle_smooth_scroll_buffers(ctx: &mut Ctx) {
    if let Some(state) = RealmServices::<RefCell<SmoothScrolls>>::current(ctx) {
        let mut state = state.borrow_mut();
        if state.active.is_empty() {
            state.active = HashMap::new();
            state.snapshot = Vec::new();
            state.updates = Vec::new();
        }
    }
}

fn start_smooth_scroll(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, from: (f32, f32), to: (f32, f32), moved: bool, inherited_end: Option<f64>) -> OpResult<()> {
    let state = match RealmServices::<RefCell<SmoothScrolls>>::current(ctx) {
        Some(state) => state,
        None => {
            let state = Rc::new(RefCell::new(SmoothScrolls::default()));
            RealmServices::replace_shared_current(ctx, state.clone());
            state
        }
    };
    let mut state = state.borrow_mut();
    state.active.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "smooth scroll allocation failed"))?;
    state.generation = state.generation.checked_add(1)
        .ok_or_else(|| OpError::new("QuotaExceededError", "smooth scroll generation exhausted"))?;
    let generation = state.generation;
    let start_ms = lumen_host::perf::web_now_ms();
    // A replacement is a new request, even for the same destination. Our UA
    // timing policy retains that motion's finish time: repeated scroll-event
    // requests cannot keep postponing arrival. New destinations get a fresh
    // duration. Preserve the active buffers across replacement as well.
    let end_ms = inherited_end.unwrap_or(start_ms + SMOOTH_SCROLL_DURATION_MS).max(start_ms);
    state.active.insert(node, SmoothScroll { owner: Rc::downgrade(realm), from, to,
        start_ms, end_ms, generation, moved });
    Ok(())
}

/// Sample real scrolling before author animation callbacks. Mutations and
/// replacement requests during a layout flush cannot restore an old request.
pub(crate) fn advance_smooth_scrolls(ctx: &mut Ctx, timestamp_ms: f64) -> OpResult<()> {
    let Some(state) = RealmServices::<RefCell<SmoothScrolls>>::current(ctx) else { return Ok(()); };
    let (mut snapshot, mut updates) = {
        let mut state = state.borrow_mut();
        let mut snapshot = core::mem::take(&mut state.snapshot);
        let mut updates = core::mem::take(&mut state.updates);
        snapshot.clear();
        updates.clear();
        snapshot.try_reserve(state.active.len()).map_err(|_| OpError::new("QuotaExceededError", "smooth scroll frame allocation failed"))?;
        updates.try_reserve(state.active.len()).map_err(|_| OpError::new("QuotaExceededError", "smooth scroll update allocation failed"))?;
        snapshot.extend(state.active.iter().map(|(node, record)| (*node, record.clone())));
        snapshot.sort_unstable_by_key(|(_, record)| (record.owner.as_ptr() as usize, record.generation));
        (snapshot, updates)
    };
    let result = (|| {
        let mut first = 0;
        while first < snapshot.len() {
            let owner_pointer = snapshot[first].1.owner.as_ptr();
            let end = first + snapshot[first..].partition_point(|(_, record)| record.owner.as_ptr() == owner_pointer);
            let group = &snapshot[first..end];
            first = end;
            let Some(owner) = group[0].1.owner.upgrade().filter(|owner| !owner.has_browsing_context
                || owner.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, owner))) else {
                let mut state = state.borrow_mut();
                for (node, record) in group {
                    if state.active.get(node).is_some_and(|active| active.generation == record.generation) { state.active.remove(node); }
                }
                continue;
            };
            // Capture every box's bounds before any replay can invalidate the frame.
            // Layout work is per document, rather than per animated box.
            owner.flush_layout()?;
            updates.clear();
            for (node, record) in group {
                if !state.borrow().active.get(node).is_some_and(|active| active.generation == record.generation) { continue; }
                if owner.session.borrow().document().kind(*node).is_err() {
                    state.borrow_mut().active.remove(node);
                    continue;
                }
                let progress = if timestamp_ms >= record.end_ms { 1.0 } else {
                    ((timestamp_ms - record.start_ms) / (record.end_ms - record.start_ms)).clamp(0.0, 1.0)
                };
                let eased = (progress * progress * (3.0 - 2.0 * progress)) as f32;
                let position = if progress >= 1.0 { record.to } else {
                    (record.from.0 + (record.to.0 - record.from.0) * eased,
                     record.from.1 + (record.to.1 - record.from.1) * eased)
                };
                updates.push(lumen_html::session::ScrollUpdate::new(*node, position.0, position.1));
            }
            owner.session.borrow_mut().set_scroll_offsets(&mut updates)
                .map_err(|error| OpError::new("InvalidStateError", format!("smooth scroll failed: {error:?}")))?;
            for update in &updates {
                let (moved, complete) = {
                    let mut state = state.borrow_mut();
                    let Some(record) = state.active.get_mut(&update.node) else { continue; };
                    if !update.has_scroll_box() { state.active.remove(&update.node); continue; }
                    let finished = timestamp_ms >= record.end_ms;
                    let complete = finished && (update.changed || record.moved);
                    record.moved |= update.changed;
                    if finished { state.active.remove(&update.node); }
                    (update.changed, complete)
                };
                if moved || complete { queue_scroll_notification(ctx, &owner, update.node, moved, complete)?; }
            }
            // Publish all positions together before listeners or RAF observe geometry.
            owner.flush_layout()?;
        }
        Ok(())
    })();
    snapshot.clear();
    updates.clear();
    let mut state = state.borrow_mut();
    if state.active.is_empty() { state.active = HashMap::new(); state.snapshot = Vec::new(); state.updates = Vec::new(); }
    else { state.snapshot = snapshot; state.updates = updates; }
    result
}

/// Publish automatic layout clamps through the same document-owned task and
/// coalescing state as explicit scrolling. A failed admission leaves the
/// session record pending for the next rendering opportunity.
pub(crate) fn queue_normalized_scroll_events(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    loop {
        let Some(node)=realm.session.borrow().pending_normalized_scroll_event() else {return Ok(());};
        if realm.session.borrow().document().kind(node).is_ok() {
            if let Some(context)=realm.browsing_context() {
                let handle=browsing_context::context_realm_handle(&context);
                ctx.with_host_realm(&handle,|ctx|queue_scroll_events(ctx,realm,node))
                    .map_err(browsing_context::host_realm_error)??;
            } else {queue_scroll_events(ctx,realm,node)?;}
        }
        realm.session.borrow_mut().acknowledge_normalized_scroll_event(node);
    }
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

fn resolved_scroll_behavior(requested: ScrollBehavior, computed: lumen_html::css::ScrollBehavior) -> ScrollBehavior {
    match requested {
        ScrollBehavior::Auto if computed == lumen_html::css::ScrollBehavior::Smooth => ScrollBehavior::Smooth,
        ScrollBehavior::Auto => ScrollBehavior::Instant,
        explicit => explicit,
    }
}

pub(crate) fn apply_offset(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    x: f64,
    y: f64,
    behavior: ScrollBehavior,
) -> OpResult<()> {
    if realm.has_browsing_context && !realm.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, realm)) {
        return Ok(());
    }
    if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
        return Ok(());
    }
    realm.flush_layout()?;
    let (from, to, behavior) = {
        let mut session = realm.session.borrow_mut();
        let root = session.document().root();
        let Some((min_x,max_x,min_y,max_y)) = session
            .scroll_bounds(node)
            .or_else(|| (node == root).then_some((0.0,0.0,0.0,0.0)))
        else {
            return Ok(());
        };
        let x = if x.is_finite() { x } else { 0.0 };
        let y = if y.is_finite() { y } else { 0.0 };
        let x=x.clamp(f64::from(min_x),f64::from(max_x)) as f32;
        let y=y.clamp(f64::from(min_y),f64::from(max_y)) as f32;
        let behavior = if behavior == ScrollBehavior::Auto {
            // The viewport uses the root element's property. Body values do
            // not propagate here, unlike the separate overflow propagation.
            let style_node = if node == root {
                selector::document_element(session.document()).unwrap_or(node)
            } else { node };
            let style = session.computed_style(style_node).map_err(|error| {
                OpError::new("InvalidStateError", format!("scroll style failed: {error:?}"))
            })?;
            resolved_scroll_behavior(behavior, style.scroll_behavior)
        } else { behavior };
        (session.scroll_offset(node), (x, y), behavior)
    };
    let apply = |ctx: &mut Ctx| {
        // Abort even when the replacement destination equals the current
        // offset: a no-op instant request must stop an ongoing animation.
        let stopped = abort_smooth_scroll(ctx, node);
        let stopped_motion = stopped.as_ref().is_some_and(|record| record.moved);
        if from == to {
            release_idle_smooth_scroll_buffers(ctx);
            if stopped_motion { queue_scroll_notification(ctx, realm, node, false, true)?; }
            return Ok(());
        }
        if behavior == ScrollBehavior::Smooth {
            let inherited_end = stopped.filter(|record| record.to == to).map(|record| record.end_ms);
            let result = start_smooth_scroll(ctx, realm, node, from, to, stopped_motion, inherited_end);
            if result.is_err() { release_idle_smooth_scroll_buffers(ctx); }
            return result;
        }
        release_idle_smooth_scroll_buffers(ctx);
        let changed = realm.session.borrow_mut().set_scroll_offset(node, to.0, to.1).map_err(|error| {
            OpError::new("InvalidStateError", format!("scroll failed: {error:?}"))
        })?;
        if changed { queue_scroll_events(ctx, realm, node)?; }
        Ok(())
    };
    if let Some(context) = realm.browsing_context() {
        let handle = browsing_context::context_realm_handle(&context);
        ctx.with_host_realm(&handle, apply).map_err(browsing_context::host_realm_error)?
    } else {
        apply(ctx)
    }
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
                session.scroll_bounds(node),
                session.scroll_offset(node),
            )
        };
        let Some((min_x,max_x,min_y,max_y)) = extent else {
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
        let attempted = (x_scrollable && remaining_x != 0.0) || (y_scrollable && remaining_y != 0.0);
        let mut next_x = f64::from(offset.0);
        let mut next_y = f64::from(offset.1);
        if x_scrollable && remaining_x != 0.0 {
            let candidate=(next_x+remaining_x).clamp(f64::from(min_x),f64::from(max_x));
            remaining_x -= candidate - next_x;
            next_x = candidate;
        }
        if y_scrollable && remaining_y != 0.0 {
            let candidate=(next_y+remaining_y).clamp(f64::from(min_y),f64::from(max_y));
            remaining_y -= candidate - next_y;
            next_y = candidate;
        }
        if attempted {
            apply_offset(ctx, realm, node, next_x, next_y, ScrollBehavior::Instant)?;
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

/// Rebuild the original target's geometry for each scroll step. In particular,
/// an ancestor scroll changes the embedded viewport's position, not the target
/// identity. Affine composition precedes bounds construction.
fn target_bounds_in_scroll_space(
    original: &Rc<DomRealm>, target: NodeId, destination: &Rc<DomRealm>,
    inverse_scroll_space: lumen_html::paint::Affine,
    projected: &[(Rc<DomRealm>, (f32, f32))],
) -> OpResult<Option<lumen_html::paint::Rect>> {
    use lumen_html::paint::{Affine, Rect};
    let mut current = original.clone();
    let translation = |realm: &Rc<DomRealm>| {
        let (e, f) = projected.iter().find(|(owner, _)| Rc::ptr_eq(owner, realm)).map_or((0.0, 0.0), |(_, delta)| *delta);
        Affine { e, f, ..Affine::IDENTITY }
    };
    let mut matrix = translation(original);
    while !Rc::ptr_eq(&current, destination) {
        let Some(context) = current.browsing_context() else { return Ok(None); };
        if !browsing_context::is_active_document(&context, &current) { return Ok(None); }
        let Some((parent, owner, frame)) = browsing_context::context_frame_element(&context) else { return Ok(None); };
        if !browsing_context::is_active_document(&parent, &owner) { return Ok(None); }
        if owner.layout_flusher.borrow().is_none() && owner.session.borrow().viewport_size().is_none() {
            return Ok(None);
        }
        owner.flush_layout()?;
        let embedding = {
            let mut session = owner.session.borrow_mut();
            if session.document().root_node(frame, true).map_err(dom_error)? != session.document().root() { return Ok(None); }
            session.content_viewport_transform(frame)
        };
        let Some(embedding) = embedding else { return Ok(None); };
        matrix = translation(&owner).then(embedding).then(matrix);
        current = owner;
    }
    original.flush_layout()?;
    let matrix = inverse_scroll_space.then(matrix);
    let session = original.session.borrow();
    if session.document().root_node(target, true).map_err(dom_error)? != session.document().root() { return Ok(None); }
    let mut bounds: Option<Rect> = None;
    for corners in session.client_fragment_corners(target) {
        for (x,y) in corners {
            let (x,y) = matrix.apply(x,y);
            if !x.is_finite() || !y.is_finite() { return Ok(None); }
            bounds = Some(match bounds {
                None => Rect { x,y,width:0.0,height:0.0 },
                Some(rect) => {
                    let left=rect.x.min(x);let top=rect.y.min(y);
                    Rect { x:left,y:top,width:(rect.x+rect.width).max(x)-left,
                        height:(rect.y+rect.height).max(y)-top }
                }
            });
        }
    }
    Ok(bounds)
}

pub(crate) fn into_view_inner(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    options: IntoViewOptions,
) -> OpResult<()> {
    if realm.has_browsing_context && !realm.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, realm)) {
        return Ok(());
    }
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
    let original = realm.clone();
    let origin = original.document_origin();
    let mut current = realm.clone();
    let mut ancestor = Some(target);
    // An inner smooth scroll has not moved the live layout yet. Align outer
    // scrollports against its already admitted destination, in each document's
    // real transformed axes, without publishing fictional current offsets.
    let mut projected: Vec<(Rc<DomRealm>, (f32, f32))> = Vec::new();
    while let Some(node) = ancestor {
        if current.browsing_context().is_some_and(|context| !browsing_context::is_active_document(&context, &current)) {
            break;
        }
        if !Rc::ptr_eq(&original, &current) && !origin.as_ref().zip(current.document_origin().as_ref())
            .is_some_and(|(source, destination)| source.same_origin(destination)) { break; }
        if current.layout_flusher.borrow().is_none() && current.session.borrow().viewport_size().is_none() {
            break;
        }
        // A fixed subtree can scroll local containers, but its own viewport
        // cannot move it. Its embedding frame can still move in another document.
        current.flush_layout()?;
        let fixed = current.session.borrow().is_viewport_fixed(node);
        let next = current
            .session
            .borrow()
            .document()
            .composed_parent(node)
            .map_err(dom_error)?;
        // The target's own scrollport does not scroll its border box.
        if node != target || !Rc::ptr_eq(&original, &current) {
            let geometry = {
                let mut session = current.session.borrow_mut();
                match session.scrollport_coordinate_space(node) {
                    Some((port, inverse)) => {
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
                            port,
                            inverse,
                            session.scroll_offset(node),
                            style.writing_mode,
                            style.direction,
                            resolved_scroll_behavior(options.behavior, style.scroll_behavior),
                        ))
                    }
                    None => None,
                }
            };
            if let Some((port, inverse, (x, y), writing_mode, direction, behavior)) = geometry {
                let Some(bounds) = target_bounds_in_scroll_space(&original, target, &current, inverse, &projected)? else { break; };
                use lumen_common::scroll::alignment_delta;
                use lumen_html::css::{Direction, WritingMode};
                let rtl = direction == Direction::Rtl;
                let (x_align, y_align, x_reverse, y_reverse) = match writing_mode {
                    WritingMode::HorizontalTb => (options.inline, options.block, rtl, false),
                    WritingMode::VerticalRl => (options.block, options.inline, true, rtl),
                    WritingMode::VerticalLr => (options.block, options.inline, false, rtl),
                    WritingMode::SidewaysRl => (options.block, options.inline, true, rtl),
                    WritingMode::SidewaysLr => (options.block, options.inline, false, !rtl),
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
                    &current,
                    node,
                    f64::from(x) + dx,
                    f64::from(y) + dy,
                    behavior,
                )?;
                if behavior == ScrollBehavior::Smooth {
                    let destination = |ctx: &mut Ctx| RealmServices::<RefCell<SmoothScrolls>>::current(ctx)
                        .and_then(|state| state.borrow().active.get(&node).map(|record| record.to));
                    let to = if let Some(context) = current.browsing_context() {
                        ctx.with_host_realm(&browsing_context::context_realm_handle(&context), destination)
                            .map_err(browsing_context::host_realm_error)?
                    } else { destination(ctx) };
                    if let (Some(to), Some(matrix)) = (to, inverse.inverse()) {
                        let dx = x - to.0; let dy = y - to.1;
                        let delta = (matrix.a * dx + matrix.c * dy, matrix.b * dx + matrix.d * dy);
                        if let Some((_, old)) = projected.iter_mut().find(|(owner, _)| Rc::ptr_eq(owner, &current)) {
                            old.0 += delta.0; old.1 += delta.1;
                        } else {
                            projected.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "scroll alignment allocation failed"))?;
                            projected.push((current.clone(), delta));
                        }
                    }
                }
                if options.nearest_container {
                    break;
                }
            }
        }
        if let Some(next) = next.filter(|_| !fixed) {
            ancestor = Some(next);
        } else {
            let Some(context) = current.browsing_context() else { break; };
            let Some((parent, owner, frame)) = browsing_context::context_frame_element(&context) else { break; };
            if !browsing_context::is_active_document(&parent, &owner) { break; }
            current = owner;
            ancestor = Some(frame);
        }
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
    fn specification_window_scroll_into_view_maps_original_target_through_frame_content_and_scale() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<style>html,body{margin:0}#outer{overflow:auto;width:100px;height:40px}iframe{display:block;width:100px;height:100px;border:5px solid;padding:7px;transform:scale(2);transform-origin:0 0}</style><div id='outer'><div style='height:200px'></div></div>", 256).unwrap();
        parent.set_document_url("https://scroll.test/parent");
        parent.set_layout_flusher(Rc::new(|session| session.display_list(500, 500, &NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        script(&mut engine, "const frame=document.createElement('iframe'); frame.src='https://scroll.test/child'; document.getElementById('outer').append(frame); true");
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child=frame.install_response_for_request(engine.ctx(),&frame.navigation_request(),"https://scroll.test/child","text/html",
            "<style>html,body{margin:0}body{height:200px}#target{height:10px;width:10px}</style><div style='height:60px'></div><div id='target'></div>",128).unwrap();
        child.set_layout_flusher(Rc::new(|session| session.display_list(100,100,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let target={let session=child.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"target").unwrap().unwrap()};
        let child_root=child.session.borrow().document().root();
        let outer={let session=parent.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"outer").unwrap().unwrap()};
        let options=IntoViewOptions { behavior:ScrollBehavior::Instant,
            block:ScrollAlignment::Center,inline:ScrollAlignment::Nearest,nearest_container:false };
        into_view_inner(engine.ctx(),&child,target,IntoViewOptions{nearest_container:true,..options}).unwrap();
        assert_eq!(child.session.borrow().scroll_offset(child_root).1,15.0);
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,0.0,"nearest viewport stops before the embedding port");
        apply_offset(engine.ctx(),&child,child_root,0.0,0.0,ScrollBehavior::Instant).unwrap();
        into_view_inner(engine.ctx(),&child,target,options).unwrap();
        assert_eq!(child.session.borrow().scroll_offset(child_root).1,15.0,"inner viewport centers the original element");
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,304.0,"outer port centers the transformed child target, including border and padding");
        parent.flush_layout().unwrap();
        let inverse=parent.session.borrow().scrollport_coordinate_space(outer).unwrap().1;
        let bounds=target_bounds_in_scroll_space(&child,target,&parent,inverse,&[]).unwrap().unwrap();
        assert!((bounds.y+bounds.height/2.0-20.0).abs()<0.01,"mapped target center {bounds:?}");
        apply_offset(engine.ctx(),&parent,outer,0.0,0.0,ScrollBehavior::Instant).unwrap();
        apply_offset(engine.ctx(),&child,child_root,0.0,0.0,ScrollBehavior::Instant).unwrap();
        engine.eval_value_in_host_realm(&frame.realm_handle(),"const fixed=document.getElementById('target');fixed.style.position='fixed';fixed.style.top='60px';",false).unwrap().ok().expect("make target viewport fixed");
        into_view_inner(engine.ctx(),&child,target,options).unwrap();
        assert_eq!(child.session.borrow().scroll_offset(child_root).1,0.0,"scrolling the child's viewport cannot move a fixed target");
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,334.0,"a fixed child target can still be revealed by its outer embedding port");
        apply_offset(engine.ctx(),&parent,outer,0.0,0.0,ScrollBehavior::Instant).unwrap();
        script(&mut engine,"frame.style.padding='10%';true");
        parent.flush_layout().unwrap();
        let owner=frame.owner_node();
        {
            let mut session=parent.session.borrow_mut();
            assert_eq!(geometry::content_box_size(&mut session,owner),Some((100.0,100.0)),"percentage padding does not enlarge the embedded content viewport");
            let snapshot=geometry::snapshot(&mut session,owner).unwrap();
            assert_eq!((snapshot.client_width,snapshot.client_height),(120.0,120.0),"client dimensions use percentage padding resolved against the actual 100px containing block");
        }
        into_view_inner(engine.ctx(),&child,target,options).unwrap();
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,340.0,"embedding translation uses the same percentage-resolved padding as its live viewport and client dimensions");
    }

    #[test]
    fn specification_window_scroll_into_view_stops_at_nearest_origin_and_retired_owner() {
        let mut engine=Engine::new();
        let parent=crate::install(engine.ctx(),"<style>html,body{margin:0}#outer{overflow:auto;height:40px;width:100px}iframe{display:block;border:0;width:100px;height:80px}</style><div id='outer'><div style='height:200px'></div></div>",256).unwrap();
        parent.set_document_url("https://parent.test/page");
        parent.set_layout_flusher(Rc::new(|session|session.display_list(400,400,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        script(&mut engine,"const frame=document.createElement('iframe');frame.src='https://foreign.test/page';document.getElementById('outer').append(frame);true");
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child=frame.install_response_for_request(engine.ctx(),&frame.navigation_request(),"https://foreign.test/page","text/html",
            "<style>html,body{margin:0}body{height:200px}#target{width:10px;height:10px}</style><div style='height:100px'></div><div id='target'></div>",128).unwrap();
        child.set_layout_flusher(Rc::new(|session|session.display_list(100,80,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let target={let session=child.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"target").unwrap().unwrap()};
        let outer={let session=parent.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"outer").unwrap().unwrap()};
        let options=IntoViewOptions {behavior:ScrollBehavior::Instant,block:ScrollAlignment::Start,
            inline:ScrollAlignment::Nearest,nearest_container:false};
        into_view_inner(engine.ctx(),&child,target,options).unwrap();
        assert_eq!(child.session.borrow().scroll_offset(child.session.borrow().document().root()).1,100.0);
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,0.0,"foreign target must not scroll its embedding document");
        // A retained old Document remains readable, but is not an active scroll
        // source after its actual embedding element is disconnected.
        script(&mut engine,"frame.remove();true");
        let before=child.session.borrow().scroll_offset(child.session.borrow().document().root());
        into_view_inner(engine.ctx(),&child,target,options).unwrap();
        assert_eq!(child.session.borrow().scroll_offset(child.session.borrow().document().root()),before);
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,0.0);
    }

    #[test]
    fn specification_window_scroll_into_view_finishes_local_scroll_without_ancestor_layout() {
        let mut engine=Engine::new();
        let parent=crate::install(engine.ctx(),"<iframe src='https://unrendered.test/child'></iframe>",128).unwrap();
        parent.set_document_url("https://unrendered.test/parent");
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child=frame.install_response_for_request(engine.ctx(),&frame.navigation_request(),"https://unrendered.test/child","text/html",
            "<style>html,body{margin:0}#target{height:10px;width:10px}</style><div style='height:100px'></div><div id='target'></div>",128).unwrap();
        child.set_layout_flusher(Rc::new(|session|session.display_list(100,50,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let target={let session=child.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"target").unwrap().unwrap()};
        into_view_inner(engine.ctx(),&child,target,IntoViewOptions{behavior:ScrollBehavior::Instant,
            block:ScrollAlignment::Start,inline:ScrollAlignment::Nearest,nearest_container:false}).unwrap();
        let session=child.session.borrow();
        assert_eq!(session.scroll_offset(session.document().root()).1,60.0,"local viewport movement completes before reaching an unrendered ancestor");
        drop(session);
        assert!(target_bounds_in_scroll_space(&child,target,&parent,lumen_html::paint::Affine::IDENTITY,&[]).unwrap().is_none());
        assert!(parent.session.borrow().viewport_size().is_none(),"scrolling must not fabricate a parent CSS box or viewport");
    }

    #[test]
    fn specification_window_scroll_into_view_recomputes_scrolled_fixed_subtree_before_embedding() {
        let mut engine=Engine::new();
        let parent=crate::install(engine.ctx(),"<style>html,body{margin:0}#outer{overflow:auto;width:100px;height:40px}iframe{display:block;border:0;width:100px;height:100px}</style><div id='outer'><div style='height:200px'></div><iframe src='https://fixed.test/child'></iframe><div style='height:500px'></div></div>",256).unwrap();
        parent.set_document_url("https://fixed.test/parent");
        parent.set_layout_flusher(Rc::new(|session|session.display_list(500,500,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child=frame.install_response_for_request(engine.ctx(),&frame.navigation_request(),"https://fixed.test/child","text/html",
            "<style>html,body{margin:0}#port{position:fixed;right:10px;bottom:10px;width:20px;height:20px;overflow:auto}#target{position:absolute;left:200%;top:200%;width:10px;height:10px}</style><div id='port'><div id='target'></div></div>",128).unwrap();
        child.set_layout_flusher(Rc::new(|session|session.display_list(100,100,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let (target,port)={let session=child.session.borrow();let doc=session.document();
            (selector::get_element_by_id(doc,doc.root(),"target").unwrap().unwrap(),selector::get_element_by_id(doc,doc.root(),"port").unwrap().unwrap())};
        into_view_inner(engine.ctx(),&child,target,IntoViewOptions{behavior:ScrollBehavior::Instant,
            block:ScrollAlignment::Start,inline:ScrollAlignment::Start,nearest_container:false}).unwrap();
        let session=child.session.borrow();
        assert_eq!(session.scroll_offset(port),(30.0,30.0));
        assert_eq!(session.scroll_offset(session.document().root()),(0.0,0.0),"fixed subtree skips the child viewport");
        drop(session);
        let session=parent.session.borrow();let doc=session.document();
        let outer=selector::get_element_by_id(doc,doc.root(),"outer").unwrap().unwrap();
        assert_eq!(session.scroll_offset(outer).1,280.0,"outer geometry must use the target after its local fixed scrollport moved; child target={:?}; port={:?}",child.session.borrow().client_rects(target),child.session.borrow().scrollport_coordinate_space(port));
    }

    #[test]
    fn specification_window_scroll_into_view_uses_scroll_box_axes_and_nested_document_chain() {
        let (mut engine, realm)=setup("<style>html,body{margin:0}#port{overflow:auto;width:30px;height:20px;transform:scale(2);transform-origin:0 0}#target{height:10px;width:10px}</style><div id='port'><div style='height:100px'></div><div id='target'></div></div>");
        assert!(matches!(script(&mut engine,"document.getElementById('target').scrollIntoView({behavior:'instant',block:'start'});document.getElementById('port').scrollTop===90"),Value::Bool(true)),"scaled scroll box offsets remain CSS pixels");
        drop(realm);
        let mut engine=Engine::new();
        let parent=crate::install(engine.ctx(),"<style>html,body{margin:0}#outer{overflow:auto;width:100px;height:40px}iframe{display:block;border:0;width:100px;height:80px}</style><div id='outer'><div style='height:200px'></div></div>",256).unwrap();
        parent.set_document_url("https://nested.test/parent");
        parent.set_layout_flusher(Rc::new(|session|session.display_list(500,500,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        script(&mut engine,"const frame=document.createElement('iframe');frame.src='https://nested.test/child';document.getElementById('outer').append(frame);true");
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child=frame.install_response_for_request(engine.ctx(),&frame.navigation_request(),"https://nested.test/child","text/html",
            "<style>html,body{margin:0}body{height:300px}iframe{display:block;border:0;width:100px;height:40px}</style><div style='height:100px'></div><iframe src='https://nested.test/grandchild'></iframe>",128).unwrap();
        child.set_layout_flusher(Rc::new(|session|session.display_list(100,80,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let nested=engine.ctx().with_host_realm(&frame.realm_handle(),|ctx|child.frame_contexts(ctx)).unwrap().unwrap().remove(0);
        let grandchild=nested.install_response_for_request(engine.ctx(),&nested.navigation_request(),"https://nested.test/grandchild","text/html",
            "<style>html,body{margin:0}body{height:100px}#target{height:10px;width:10px}</style><div style='height:20px'></div><div id='target'></div>",128).unwrap();
        grandchild.set_layout_flusher(Rc::new(|session|session.display_list(100,40,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let target={let session=grandchild.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"target").unwrap().unwrap()};
        into_view_inner(engine.ctx(),&grandchild,target,IntoViewOptions{behavior:ScrollBehavior::Instant,block:ScrollAlignment::Center,inline:ScrollAlignment::Nearest,nearest_container:false}).unwrap();
        assert_eq!(grandchild.session.borrow().scroll_offset(grandchild.session.borrow().document().root()).1,5.0);
        assert_eq!(child.session.borrow().scroll_offset(child.session.borrow().document().root()).1,80.0);
        let outer={let session=parent.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),"outer").unwrap().unwrap()};
        assert_eq!(parent.session.borrow().scroll_offset(outer).1,220.0,"both embedded viewport origins participate in ancestor alignment");
    }

    #[test]
    fn specification_window_scroll_microtasks_admit_next_document_phase_after_root_height_scroll() {
        let (mut engine, realm) = setup("<!doctype html><style>html{height:300vh;width:100%}body{margin:0}</style>");
        script(&mut engine, "var scrollPhases=[];addEventListener('scroll',()=>scrollPhases.push(scrollY));addEventListener('scrollend',()=>scrollPhases.push('end'));(async()=>{let once=()=>new Promise(resolve=>addEventListener('scroll',resolve,{once:true}));let p=once();document.scrollingElement.scrollTop=100;await p;p=once();document.scrollingElement.scrollTop=150;await p;scrollPhases.push('done')})()");
        let root = realm.session.borrow().document().root();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 100.0, "the root element's own height contributes to viewport overflow");
        run_scroll_steps(engine.ctx()).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 150.0);
        assert!(scroll_events_pending(engine.ctx()), "the resumed promise's scroll survives the current ordered snapshot");
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollPhases.filter(v=>typeof v==='number').join(',')==='100,150'&&scrollPhases.includes('done')&&scrollPhases[scrollPhases.length-1]==='end'"), Value::Bool(true)));
    }

    #[test]
    fn specification_window_scroll_handler_attributes_share_live_trusted_event_slots() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}body{height:500px}#port{overflow:auto;width:40px;height:40px}</style><div id=port onscroll='scrollHandlers.push([event.type,event.isTrusted,this.id])' onscrollend='scrollHandlers.push([event.type,event.isTrusted,this.id])'><div style='height:300px'></div></div>");
        assert!(matches!(script(&mut engine, "var scrollHandlers=[];var port=document.getElementById('port');typeof port.onscroll==='function'&&typeof port.onscrollend==='function'&&document.onscroll===null&&document.onscrollend===null&&window.onscroll===null&&window.onscrollend===null"), Value::Bool(true)));
        script(&mut engine, "document.onscroll=e=>scrollHandlers.push([e.type,e.isTrusted,'document']);document.onscrollend=e=>scrollHandlers.push([e.type,e.isTrusted,'document']);window.onscroll=e=>scrollHandlers.push([e.type,e.isTrusted,'window']);window.onscrollend=e=>scrollHandlers.push([e.type,e.isTrusted,'window']);port.scrollTop=10;scrollTo({top:20,behavior:'instant'})");
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollHandlers.length===6&&scrollHandlers.every(e=>e[1])&&scrollHandlers.filter(e=>e[0]==='scroll').length===3&&scrollHandlers.filter(e=>e[0]==='scrollend').length===3"), Value::Bool(true)));
        script(&mut engine, "scrollHandlers=[];port.onscrollend=null;port.scrollTop=20");
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollHandlers.length===1&&scrollHandlers[0][0]==='scroll'&&port.onscrollend===null"), Value::Bool(true)));
        script(&mut engine, r#"port.setAttribute('onscrollend',"scrollHandlers.push([event.type,event.isTrusted,this.id])");port.scrollTop=30"#);
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollHandlers.length===3&&scrollHandlers[2][0]==='scrollend'&&scrollHandlers[2][2]==='port'"), Value::Bool(true)));
        assert_eq!(realm.session.borrow().scroll_offset(realm.session.borrow().document().root()).1, 20.0);
    }

    #[test]
    fn specification_window_scroll_behavior_uses_associated_element_and_preserves_user_scroll() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}body{height:500px;scroll-behavior:smooth}#port{overflow:auto;width:40px;height:40px;scroll-behavior:smooth}#content{height:300px}</style><div id=port><div id=content></div></div>");
        script(&mut engine, "scrollTo({top:20,behavior:'auto'})");
        let root = realm.session.borrow().document().root();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 20.0, "body smooth behavior does not propagate to the viewport");
        assert!(!smooth_scroll_pending(engine.ctx()));
        script(&mut engine, "document.documentElement.style.scrollBehavior='smooth';scrollTo({top:100,behavior:'auto'})");
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 20.0);
        assert!(smooth_scroll_pending(engine.ctx()));
        script(&mut engine, "scrollTo({top:30,behavior:'instant'})");
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 30.0);
        assert!(!smooth_scroll_pending(engine.ctx()));
        script(&mut engine, "document.getElementById('port').scrollTop=80");
        let port = { let session = realm.session.borrow(); selector::get_element_by_id(session.document(), session.document().root(), "port").unwrap().unwrap() };
        assert_eq!(realm.session.borrow().scroll_offset(port).1, 0.0, "scrollTop uses auto and honors the port's property");
        let state = RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
        assert_eq!(state.borrow().active[&port].to.1, 80.0);
        apply_wheel_default_action(engine.ctx(), &realm, port, 0.0, 10.0).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(port).1, 10.0, "wheel movement uses user scrolling rather than CSSOM behavior");
        assert!(!smooth_scroll_pending(engine.ctx()));
        script(&mut engine, "document.getElementById('port').scrollTo({top:50,behavior:'instant'})");
        assert_eq!(realm.session.borrow().scroll_offset(port).1, 50.0);
    }

    #[test]
    fn specification_window_smooth_scroll_into_view_projects_nested_transformed_destinations() {
        for (scale, behavior) in [("none", ScrollBehavior::Smooth), ("scale(2)", ScrollBehavior::Smooth), ("scale(2)", ScrollBehavior::Auto)] {
            let (mut engine, realm) = setup(&format!("<!doctype html><style>html,body{{margin:0}}#outer{{overflow:auto;width:90px;height:50px;scroll-behavior:smooth}}#inner{{overflow:auto;width:40px;height:40px;margin-top:100px;transform:{scale};transform-origin:0 0;scroll-behavior:smooth}}#target{{height:10px}}</style><div id=outer><div id=inner><div style='height:80px'></div><div id=target></div><div style='height:200px'></div></div><div style='height:200px'></div></div>"));
            let lookup = |id| { let session = realm.session.borrow(); selector::get_element_by_id(session.document(), session.document().root(), id).unwrap().unwrap() };
            let inner = lookup("inner"); let outer = lookup("outer"); let target = lookup("target");
            into_view_inner(engine.ctx(), &realm, target, IntoViewOptions { behavior,
                block: ScrollAlignment::Start, inline: ScrollAlignment::Nearest, nearest_container: false }).unwrap();
            assert_eq!(realm.session.borrow().scroll_offset(inner).1, 0.0, "smooth admission preserves the real current position");
            let state = RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
            assert_eq!(state.borrow().active[&inner].to.1, 80.0, "inner alignment remains CSS pixels: {scale}; port={:?}; target={:?}", realm.session.borrow().scrollport_coordinate_space(inner), realm.session.borrow().client_rects(target));
            assert_eq!(state.borrow().active[&outer].to.1, 100.0, "outer alignment accounts for the inner's pending transformed movement: {scale}");
            let end = state.borrow().active.values().map(|record| record.start_ms).fold(0.0, f64::max) + SMOOTH_SCROLL_DURATION_MS;
            advance_smooth_scrolls(engine.ctx(), end).unwrap();
            assert_eq!(realm.session.borrow().scroll_offset(inner).1, 80.0);
            assert_eq!(realm.session.borrow().scroll_offset(outer).1, 100.0);
        }
    }

    #[test]
    fn specification_window_nested_smooth_scroll_preserves_bounds_after_fractional_samples() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}#outer{width:100px;height:100px;overflow:scroll;scroll-behavior:smooth}#inner{width:200px;height:200px;overflow:scroll}#target{width:400px;height:400px}</style><div id=outer><div id=inner><div id=target></div></div></div>");
        let lookup = |id| { let session=realm.session.borrow();selector::get_element_by_id(session.document(),session.document().root(),id).unwrap().unwrap() };
        let inner=lookup("inner");let outer=lookup("outer");let target=lookup("target");
        into_view_inner(engine.ctx(),&realm,target,IntoViewOptions {behavior:ScrollBehavior::Auto,
            block:ScrollAlignment::End,inline:ScrollAlignment::End,nearest_container:false}).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(inner),(200.0,200.0));
        let state=RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
        let start=state.borrow().active[&outer].start_ms;
        for part in [0.19,0.43,0.81,1.0] {
            advance_smooth_scrolls(engine.ctx(),start+SMOOTH_SCROLL_DURATION_MS*part).unwrap();
            realm.flush_layout().unwrap();
            assert_eq!(realm.session.borrow().scroll_bounds(inner),Some((0.0,200.0,0.0,200.0)),"ancestor animation must not alter local scrolling-area dimensions");
        }
        assert_eq!(realm.session.borrow().scroll_offset(outer),(100.0,100.0));
        assert_eq!(realm.session.borrow().scroll_offset(inner),(200.0,200.0));
    }

    #[test]
    fn specification_window_smooth_scroll_reentrant_same_destination_finishes_and_reuses_storage() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}#port{width:20px;height:20px;overflow:hidden;scroll-behavior:smooth}#content{height:220px}</style><div id=port><div id=content></div></div>");
        assert!(matches!(script(&mut engine, r#"
            var port=document.getElementById('port'), replacements=0, ends=[];
            port.onscroll=()=>{if(port.scrollTop>1&&port.scrollTop<200){port.scrollTop=1;replacements++}};
            port.onscrollend=()=>ends.push(port.scrollTop);
            port.scrollTop=200;
            port.scrollTop===0
        "#),Value::Bool(true)));
        let node=selector::get_element_by_id(realm.session.borrow().document(),realm.session.borrow().document().root(),"port").unwrap().unwrap();
        let state=RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
        let (first_start,first_generation,capacity)={
            let state=state.borrow();let record=&state.active[&node];
            (record.start_ms,record.generation,state.active.capacity())
        };
        let first_sample=first_start+SMOOTH_SCROLL_DURATION_MS/5.0;
        advance_smooth_scrolls(engine.ctx(),first_sample).unwrap();
        run_scroll_steps(engine.ctx()).unwrap();
        let (deadline,generation)={
            let state=state.borrow();let record=&state.active[&node];
            assert_eq!(record.to,(0.0,1.0));
            assert_eq!(record.end_ms,record.start_ms+SMOOTH_SCROLL_DURATION_MS,"a changed destination gets a fresh duration");
            assert_eq!(state.active.capacity(),capacity,"replacement retains the existing sparse allocation");
            (record.end_ms,record.generation)
        };
        assert!(generation>first_generation,"interruption admits a genuine new request");
        for step in 1..=8 {
            let timestamp=if step==8 {deadline} else {first_sample+(deadline-first_sample)*f64::from(step)/8.0};
            advance_smooth_scrolls(engine.ctx(),timestamp).unwrap();
            run_scroll_steps(engine.ctx()).unwrap();
            let state=state.borrow();
            if let Some(record)=state.active.get(&node) {
                assert_eq!(record.end_ms,deadline,"reentrant requests for the same destination cannot postpone completion");
                assert_eq!(state.active.capacity(),capacity,"repeated replacements do not churn the sparse allocation");
            }
        }
        assert!(matches!(script(&mut engine,"replacements>=2&&port.scrollTop===1&&ends.length===1&&ends[0]===1"),Value::Bool(true)),"actual scroll listeners repeatedly replace the request and observe one exact completion");
        let state=state.borrow();
        assert!(state.active.is_empty());
        assert_eq!((state.active.capacity(),state.snapshot.capacity(),state.updates.capacity()),(0,0,0),"completed motion releases quiet storage");
    }

    #[test]
    fn specification_window_smooth_scroll_progress_abort_and_completion_use_rendering_opportunities() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}body{height:400px}</style>");
        assert!(matches!(script(&mut engine,
            "var scrollEvents=[];addEventListener('scroll',e=>scrollEvents.push(e.type+':'+e.isTrusted));addEventListener('scrollend',e=>scrollEvents.push(e.type+':'+e.isTrusted));scrollTo({top:160,behavior:'smooth'});scrollY===0&&scrollEvents.length===0"), Value::Bool(true)));
        assert!(scheduling::animation_frame_pending(engine.ctx()), "native animation work wakes the host without author RAF callbacks");
        let root = realm.session.borrow().document().root();
        let state = RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
        let start = state.borrow().active[&root].start_ms;
        advance_smooth_scrolls(engine.ctx(), start + SMOOTH_SCROLL_DURATION_MS / 2.0).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 80.0);
        assert!(matches!(script(&mut engine, "scrollEvents.length===0"), Value::Bool(true)), "scroll events are queued");
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollEvents.join(',')==='scroll:true'"), Value::Bool(true)), "intermediate samples do not finish the scroll");
        script(&mut engine, "scrollTo({top:scrollY,behavior:'instant'})");
        assert!(!smooth_scroll_pending(engine.ctx()), "an unchanged instant destination still aborts smooth scrolling");
        advance_smooth_scrolls(engine.ctx(), start + SMOOTH_SCROLL_DURATION_MS).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 80.0);
        script(&mut engine, "scrollTo({top:180,behavior:'smooth'})");
        let start = state.borrow().active[&root].start_ms;
        advance_smooth_scrolls(engine.ctx(), start + SMOOTH_SCROLL_DURATION_MS).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 180.0);
        assert!(!smooth_scroll_pending(engine.ctx()));
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollEvents.join(',')==='scroll:true,scroll:true,scrollend:true'"), Value::Bool(true)));
        apply_offset(engine.ctx(), &realm, root, 0.0, 320.0, ScrollBehavior::Instant).unwrap();
        apply_offset(engine.ctx(), &realm, root, 0.0, 0.0, ScrollBehavior::Smooth).unwrap();
        apply_wheel_default_action(engine.ctx(), &realm, root, 0.0, 10.0).unwrap();
        assert!(!smooth_scroll_pending(engine.ctx()), "a real wheel attempt aborts smooth scrolling even at the existing boundary");
        assert_eq!(realm.session.borrow().scroll_offset(root).1, 320.0);
    }

    #[test]
    fn specification_window_smooth_scrolls_keep_boxes_independent_and_rebind_event_ownership() {
        let (mut engine, realm) = setup("<!doctype html><style>html,body{margin:0}.port{overflow:auto;width:20px;height:20px}.content{height:200px}</style><div id=a class=port><div class=content></div></div><div id=b class=port><div class=content></div></div>");
        script(&mut engine, "var scrollEvents=[];for(const id of ['a','b'])document.getElementById(id).addEventListener('scrollend',()=>scrollEvents.push(id))");
        let lookup = |id| { let session = realm.session.borrow(); selector::get_element_by_id(session.document(), session.document().root(), id).unwrap().unwrap() };
        let a = lookup("a"); let b = lookup("b");
        apply_offset(engine.ctx(), &realm, a, 0.0, 120.0, ScrollBehavior::Smooth).unwrap();
        apply_offset(engine.ctx(), &realm, b, 0.0, 500.0, ScrollBehavior::Smooth).unwrap();
        let state = RealmServices::<RefCell<SmoothScrolls>>::current(engine.ctx()).unwrap();
        assert_eq!(state.borrow().active.len(), 2);
        assert_eq!(state.borrow().active[&b].to.1, 180.0, "destinations use real scroll bounds");
        let end = state.borrow().active.values().map(|record| record.start_ms).fold(0.0, f64::max) + SMOOTH_SCROLL_DURATION_MS;
        advance_smooth_scrolls(engine.ctx(), end).unwrap();
        assert_eq!(realm.session.borrow().scroll_offset(a).1, 120.0);
        assert_eq!(realm.session.borrow().scroll_offset(b).1, 180.0);
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollEvents.length===2&&scrollEvents.includes('a')&&scrollEvents.includes('b')"), Value::Bool(true)));
        script(&mut engine, "scrollEvents=[]");
        apply_offset(engine.ctx(), &realm, a, 0.0, 0.0, ScrollBehavior::Instant).unwrap();
        apply_offset(engine.ctx(), &realm, b, 0.0, 0.0, ScrollBehavior::Smooth).unwrap();
        // The same rebind called by reused-Window navigation releases old
        // records and separates old queued event closures from the new queue.
        scheduling::rebind_document(engine.ctx());
        assert!(!smooth_scroll_pending(engine.ctx()));
        apply_offset(engine.ctx(), &realm, a, 0.0, 30.0, ScrollBehavior::Instant).unwrap();
        run_scroll_steps(engine.ctx()).unwrap();
        assert!(matches!(script(&mut engine, "scrollEvents.join(',')==='a'"), Value::Bool(true)), "old event callbacks cannot consume notifications from the new document queue");
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
        assert!(scheduling::animation_frame_pending(engine.ctx()));
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        let result = script(
            &mut engine,
            "events.join(',') === 'scroll:true,scrollend:true'",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn specification_window_scroll_rendering_phase_orders_boxes_microtasks_and_same_frame_raf() {
        let (mut engine, _realm) = setup("<!doctype html><style>html,body{margin:0}.port{overflow:auto;width:20px;height:20px}.content{height:200px}</style><div id=a class=port><div class=content></div></div><div id=b class=port><div class=content></div></div>");
        script(&mut engine, r#"
            var events=[], a=document.getElementById('a'), b=document.getElementById('b');
            a.addEventListener('scroll',()=>{
                events.push('scroll:a:'+a.scrollTop);
                Promise.resolve().then(()=>events.push('microtask:a'));
                requestAnimationFrame(()=>events.push('raf:listener'));
                if(a.scrollTop===10)a.scrollTop=30;
            });
            b.addEventListener('scroll',()=>events.push('scroll:b:'+b.scrollTop));
            a.addEventListener('scrollend',()=>events.push('end:a'));
            b.addEventListener('scrollend',()=>events.push('end:b'));
            requestAnimationFrame(()=>events.push('raf:existing'));
            a.scrollTop=10;b.scrollTop=20;
            if(events.length)throw new Error('synchronous scroll delivery');
        "#);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(script(&mut engine, "events.length===0"), Value::Bool(true)), "ordinary tasks do not deliver rendering events");
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(matches!(script(&mut engine, "events.join(',')==='scroll:a:10,microtask:a,scroll:b:20,end:a,end:b,raf:existing,raf:listener'"), Value::Bool(true)), "all ordered scroll listeners and microtasks precede completed targets and same-opportunity RAF");
        assert!(scheduling::animation_frame_pending(engine.ctx()), "a listener's new scroll wakes another opportunity without RAF");
        script(&mut engine, "events=[]");
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(matches!(script(&mut engine, "events.join(',')==='scroll:a:30,microtask:a,end:a,raf:listener'"), Value::Bool(true)), "reentrant notifications use the next document snapshot");
        assert!(!scheduling::animation_frame_pending(engine.ctx()));
    }

    #[test]
    fn specification_window_scroll_events_follow_adopted_targets_and_inert_creation_realms() {
        let (mut engine, parent) = setup("<!doctype html><style>html,body{margin:0}.port{overflow:auto;width:20px;height:20px}.content{height:200px}</style><div id=a class=port><div class=content></div></div><div id=b class=port><div class=content></div></div>");
        parent.set_document_url("https://scroll.test/parent");
        script(&mut engine, "var frame=document.createElement('iframe');frame.src='https://scroll.test/child';document.body.appendChild(frame);true");
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let _child = frame.install_response_for_request(engine.ctx(), &frame.navigation_request(),
            "https://scroll.test/child", "text/html", "<!doctype html><body></body>", 64).unwrap();
        script(&mut engine, r#"
            var a=document.getElementById('a'),b=document.getElementById('b'),events=[],inert;
            const child=frame.contentWindow;
            a.addEventListener('scroll',()=>{
                events.push('scroll:a');
                let intermediate=document.implementation.createHTMLDocument('intermediate');
                intermediate.adoptNode(b);
                inert=new child.DOMParser().parseFromString('<body></body>','text/html');
                inert.body.appendChild(b);intermediate=null;
            });
            b.addEventListener('scroll',event=>{
                if(event.target!==b || b.ownerDocument!==inert || !(event instanceof child.Event) || event instanceof Event)
                    throw new Error('adopted scroll target or inert event realm');
                events.push('scroll:b');
            });
            a.addEventListener('scrollend',()=>events.push('end:a'));
            b.addEventListener('scrollend',event=>{
                if(event.target!==b || !(event instanceof child.Event))throw new Error('adopted completion realm');
                events.push('end:b');
            });
            a.scrollTop=10;b.scrollTop=20;
        "#);
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(matches!(script(&mut engine, "events.join(',')==='scroll:a,scroll:b,end:a,end:b'"), Value::Bool(true)), "original document ordering survives repeated adoption and foreign inert ownership");
        let current = engine.ctx().current_host_realm();
        assert!(current.same_realm(&browsing_context::context_realm_handle(&parent.browsing_context().unwrap())), "delivery restores the phase's source host realm");
        script(&mut engine, "events=[];a.scrollTop=30;");
        scheduling::rebind_document(engine.ctx());
        assert!(!scroll_events_pending(engine.ctx()), "document replacement cancels its retained pending targets");
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(matches!(script(&mut engine, "events.length===0"), Value::Bool(true)));
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
