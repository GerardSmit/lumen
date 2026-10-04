//! Frame-sampled ResizeObserver and IntersectionObserver geometry.
//!
//! This module deliberately has no timer or synthetic layout path. The DOM
//! adapter calls `sample_*` only after a successful retained layout. Resize
//! delivery is drained in the pre-paint phase and intersection delivery from a
//! host task.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsFunction, JsHost};
use lumen_bind::{Host, This};
use lumen_html::{paint::Rect, NodeId};
use std::{
    collections::HashMap,
    rc::{Rc, Weak},
};

/// Sample one target from the renderer's latest successful retained layout.
pub fn sample_resize(
    session: &mut lumen_html::session::RenderSession,
    state: &mut ResizeState,
    target: NodeId,
) -> Option<ResizeEntry> {
    let geometry = crate::geometry::snapshot(session, target)?;
    state.sample(target, geometry.content_rect, geometry.border_box_size)
}

/// Sample an intersection against a caller-supplied root rectangle. For the
/// implicit root, use the viewport dimensions of the actual rendered frame.
pub fn sample_intersection(
    session: &mut lumen_html::session::RenderSession,
    state: &mut IntersectionState,
    target: NodeId,
    root_rect: Rect,
) -> Option<IntersectionEntry> {
    let geometry = crate::geometry::snapshot(session, target)?;
    let clipped = if geometry.has_layout_box {
        session.overflow_clipped_bounds(target).unwrap_or(Rect {
            x: root_rect.x + root_rect.width + 1.0,
            y: root_rect.y + root_rect.height + 1.0,
            width: 0.0,
            height: 0.0,
        })
    } else {
        Rect {
            x: root_rect.x + root_rect.width + 1.0,
            y: root_rect.y + root_rect.height + 1.0,
            width: 0.0,
            height: 0.0,
        }
    };
    state.sample_clipped(
        target,
        geometry.bounding_client_rect,
        root_rect,
        Some(clipped),
    )
}

pub fn sample_viewport_intersection(
    session: &mut lumen_html::session::RenderSession,
    state: &mut IntersectionState,
    target: NodeId,
) -> Option<IntersectionEntry> {
    let (width, height) = session.viewport_size()?;
    sample_intersection(
        session,
        state,
        target,
        Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
    )
}

fn sample_observer_intersection(
    session: &mut lumen_html::session::RenderSession,
    state: &mut IntersectionState,
    target: NodeId,
    root: Option<NodeId>,
    margin: [Margin; 4],
) -> Option<IntersectionEntry> {
    let root_rect = if let Some(root) = root.filter(|root| *root != session.document().root()) {
        crate::geometry::snapshot(session, root)?.bounding_client_rect
    } else {
        let (width, height) = session.viewport_size()?;
        Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        }
    };
    let geometry = crate::geometry::snapshot(session, target)?;
    let target_rect = geometry.bounding_client_rect;
    let within_root = root.is_none_or(|root| {
        let mut current = Some(target);
        while let Some(node) = current {
            if node == root {
                return true;
            }
            current = session.document().composed_parent(node).ok().flatten();
        }
        false
    });
    let clipped = if !geometry.has_layout_box {
        Rect {
            x: root_rect.x + root_rect.width + 1.0,
            y: root_rect.y + root_rect.height + 1.0,
            width: 0.0,
            height: 0.0,
        }
    } else if !within_root {
        Rect {
            x: root_rect.x + root_rect.width + 1.0,
            y: root_rect.y + root_rect.height + 1.0,
            width: 0.0,
            height: 0.0,
        }
    } else {
        session.overflow_clipped_bounds(target).unwrap_or(Rect {
            x: target_rect.x + target_rect.width + 1.0,
            y: target_rect.y + target_rect.height + 1.0,
            width: 0.0,
            height: 0.0,
        })
    };
    state.sample_clipped(
        target,
        target_rect,
        expand_root(root_rect, margin),
        Some(clipped),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeEntry {
    pub target: NodeId,
    pub content_rect: Rect,
    pub content_box_size: (f32, f32),
    pub border_box_size: (f32, f32),
    pub device_pixel_content_box_size: (f32, f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntersectionEntry {
    pub target: NodeId,
    pub bounding_client_rect: Rect,
    pub root_bounds: Rect,
    pub intersection_rect: Rect,
    pub intersection_ratio: f64,
    pub is_intersecting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeState {
    last_observed_size: Option<(f32, f32)>,
}

impl ResizeState {
    pub const fn new() -> Self {
        Self {
            last_observed_size: None,
        }
    }

    /// Produce an entry on the initial observation and whenever content size
    /// changes. The caller supplies geometry from the current completed frame.
    pub fn sample(
        &mut self,
        target: NodeId,
        content_rect: Rect,
        border_box: (f32, f32),
    ) -> Option<ResizeEntry> {
        self.sample_with_box(
            target,
            content_rect,
            border_box,
            (content_rect.width, content_rect.height),
            (content_rect.width, content_rect.height),
        )
    }

    pub fn sample_with_box(
        &mut self,
        target: NodeId,
        content_rect: Rect,
        border_box: (f32, f32),
        observed_size: (f32, f32),
        device_pixel_content_box: (f32, f32),
    ) -> Option<ResizeEntry> {
        if !content_rect.is_valid() || !border_box.0.is_finite() || !border_box.1.is_finite() {
            return None;
        }
        if !device_pixel_content_box.0.is_finite() || !device_pixel_content_box.1.is_finite() {
            return None;
        }
        let content_box_size = (content_rect.width, content_rect.height);
        let size = observed_size;
        if self.last_observed_size == Some(size) {
            return None;
        }
        self.last_observed_size = Some(size);
        Some(ResizeEntry {
            target,
            content_rect,
            content_box_size,
            border_box_size: border_box,
            device_pixel_content_box_size: device_pixel_content_box,
        })
    }

    pub fn reset(&mut self) {
        self.last_observed_size = None;
    }
}

impl Default for ResizeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntersectionState {
    thresholds: Vec<f64>,
    previous_ratio: Option<f64>,
    previous_intersecting: Option<bool>,
}

impl IntersectionState {
    pub fn new(mut thresholds: Vec<f64>) -> Result<Self, &'static str> {
        if thresholds
            .iter()
            .any(|threshold| !threshold.is_finite() || !(0.0..=1.0).contains(threshold))
        {
            return Err("intersection thresholds must be finite values between 0 and 1");
        }
        thresholds.sort_by(f64::total_cmp);
        thresholds.dedup_by(|a, b| a == b);
        if thresholds.is_empty() {
            thresholds.push(0.0);
        }
        Ok(Self {
            thresholds,
            previous_ratio: None,
            previous_intersecting: None,
        })
    }

    /// Sample one target against an explicit root rectangle. A caller should
    /// pass the viewport rectangle for the implicit root, or a root element's
    /// transformed client rectangle for an explicit root.
    pub fn sample(
        &mut self,
        target: NodeId,
        target_rect: Rect,
        root_rect: Rect,
    ) -> Option<IntersectionEntry> {
        self.sample_clipped(target, target_rect, root_rect, None)
    }

    pub fn sample_clipped(
        &mut self,
        target: NodeId,
        target_rect: Rect,
        root_rect: Rect,
        ancestor_clip: Option<Rect>,
    ) -> Option<IntersectionEntry> {
        if !target_rect.is_valid() || !root_rect.is_valid() {
            return None;
        }
        let root_intersection = target_rect.intersection(root_rect).unwrap_or(Rect {
            x: target_rect.x.max(root_rect.x),
            y: target_rect.y.max(root_rect.y),
            width: 0.0,
            height: 0.0,
        });
        let intersection = ancestor_clip
            .and_then(|clip| root_intersection.intersection(clip))
            .unwrap_or_else(|| {
                if ancestor_clip.is_some() {
                    Rect {
                        x: root_intersection.x.max(ancestor_clip.unwrap().x),
                        y: root_intersection.y.max(ancestor_clip.unwrap().y),
                        width: 0.0,
                        height: 0.0,
                    }
                } else {
                    root_intersection
                }
            });
        let area = f64::from(target_rect.width) * f64::from(target_rect.height);
        let intersection_area = f64::from(intersection.width) * f64::from(intersection.height);
        let touches = |a: Rect, b: Rect| {
            a.x <= b.x + b.width
                && a.x + a.width >= b.x
                && a.y <= b.y + b.height
                && a.y + a.height >= b.y
        };
        let is_intersecting = touches(target_rect, root_rect)
            && ancestor_clip
                .is_none_or(|clip| touches(target_rect, clip) && touches(root_intersection, clip));
        let ratio = if area == 0.0 {
            if is_intersecting {
                1.0
            } else {
                0.0
            }
        } else {
            (intersection_area / area).clamp(0.0, 1.0)
        };
        let threshold_crossed = self.previous_ratio.is_none_or(|previous| {
            self.thresholds.iter().any(|threshold| {
                (previous < *threshold && ratio >= *threshold)
                    || (previous >= *threshold && ratio < *threshold)
            })
        });
        let state_changed = self.previous_intersecting != Some(is_intersecting);
        self.previous_ratio = Some(ratio);
        self.previous_intersecting = Some(is_intersecting);
        (threshold_crossed || state_changed).then_some(IntersectionEntry {
            target,
            bounding_client_rect: target_rect,
            root_bounds: root_rect,
            intersection_rect: intersection,
            intersection_ratio: ratio,
            is_intersecting,
        })
    }

    pub fn reset(&mut self) {
        self.previous_ratio = None;
        self.previous_intersecting = None;
    }
}

struct ResizeTarget {
    state: ResizeState,
    box_kind: ResizeBox,
    _keep: DomNodeList,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ResizeBox {
    #[default]
    ContentBox,
    BorderBox,
    DevicePixelContentBox,
}
struct IntersectionTarget {
    state: IntersectionState,
    _keep: DomNodeList,
}
struct ResizePending {
    target: NodeId,
    entry: ResizeEntry,
    _keep: DomNodeList,
}
struct IntersectionPending {
    target: NodeId,
    entry: IntersectionEntry,
    _keep: DomNodeList,
}
struct ResizeData {
    callback: JsFunction,
    targets: RefCell<HashMap<NodeId, ResizeTarget>>,
    pending: RefCell<Vec<ResizePending>>,
    wrapper: RefCell<Option<WeakValue>>,
}
struct IntersectionData {
    callback: JsFunction,
    thresholds: Vec<f64>,
    root: Option<NodeId>,
    root_margin: [Margin; 4],
    root_margin_text: String,
    targets: RefCell<HashMap<NodeId, IntersectionTarget>>,
    pending: RefCell<Vec<IntersectionPending>>,
    wrapper: RefCell<Option<WeakValue>>,
}
#[derive(Clone, Copy, Debug, Default)]
struct Margin {
    pixels: f32,
    fraction: f32,
}

fn parse_root_margin(raw: &str) -> Result<([Margin; 4], String), &'static str> {
    let parts = raw.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 4 {
        return Err("rootMargin must contain one to four lengths");
    }
    let mut parsed = Vec::new();
    for part in parts {
        let (number, percent) = if let Some(value) = part.strip_suffix('%') {
            (value, true)
        } else if let Some(value) = part.strip_suffix("px") {
            (value, false)
        } else {
            return Err("rootMargin supports px and percent lengths");
        };
        let value = number
            .parse::<f32>()
            .map_err(|_| "invalid rootMargin length")?;
        if !value.is_finite() {
            return Err("rootMargin lengths must be finite");
        }
        parsed.push(if percent {
            Margin {
                pixels: 0.0,
                fraction: value / 100.0,
            }
        } else {
            Margin {
                pixels: value,
                fraction: 0.0,
            }
        });
    }
    let expanded = match parsed.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => return Err("invalid rootMargin"),
    };
    let normalized = expanded
        .iter()
        .map(|margin| {
            if margin.fraction != 0.0 {
                format!("{}%", margin.fraction * 100.0)
            } else {
                format!("{}px", margin.pixels)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok((expanded, normalized))
}

fn expand_root(rect: Rect, margins: [Margin; 4]) -> Rect {
    let resolve = |margin: Margin| margin.pixels + margin.fraction * rect.width;
    let top = resolve(margins[0]);
    let right = resolve(margins[1]);
    let bottom = resolve(margins[2]);
    let left = resolve(margins[3]);
    Rect {
        x: rect.x - left,
        y: rect.y - top,
        width: (rect.width + left + right).max(0.0),
        height: (rect.height + top + bottom).max(0.0),
    }
}
struct ResizeHub {
    realm: Weak<DomRealm>,
    observers: RefCell<Vec<Weak<ResizeData>>>,
    delivery: RefCell<Option<JsFunction>>,
    scheduled: Cell<bool>,
}
struct IntersectionHub {
    realm: Weak<DomRealm>,
    observers: RefCell<Vec<Weak<IntersectionData>>>,
    delivery: RefCell<Option<JsFunction>>,
    scheduled: Cell<bool>,
}

impl ResizeHub {
    fn schedule(&self) {
        self.scheduled.set(true);
    }
}
impl IntersectionHub {
    fn schedule(&self) {
        self.scheduled.set(true);
    }
}

#[lumen_bind::class(name = "ResizeObserver", hint(js(webidl)))]
pub struct DomResizeObserver {
    hub: Rc<ResizeHub>,
    data: Rc<ResizeData>,
}
#[lumen_bind::methods]
impl DomResizeObserver {
    #[constructor]
    fn new(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Self> {
        let hub = RealmServices::<ResizeHub>::current(ctx)
            .ok_or_else(|| OpError::new("Error", "ResizeObserver is not installed"))?;
        let data = Rc::new(ResizeData {
            callback,
            targets: RefCell::new(HashMap::new()),
            pending: RefCell::new(Vec::new()),
            wrapper: RefCell::new(None),
        });
        hub.observers.borrow_mut().push(Rc::downgrade(&data));
        Ok(Self { hub, data })
    }
    fn observe(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        target: &DomNode,
        options: Option<Value>,
    ) -> OpResult<()> {
        let realm = self
            .hub
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        if !Rc::ptr_eq(&realm, &target.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "observer target belongs to another realm",
            ));
        }
        if !matches!(
            realm.session.borrow().document().kind(target.id),
            Ok(lumen_html::NodeKind::Element { .. })
        ) {
            return Err(OpError::new(
                "TypeError",
                "ResizeObserver target must be an Element",
            ));
        }
        let box_kind = if let Some(options) = options {
            let value = ctx.get_member(&options, "box").map_err(|_| {
                OpError::new("TypeError", "ResizeObserver box option getter failed")
            })?;
            match value {
                Value::Undefined => ResizeBox::ContentBox,
                Value::Str(value) if value.as_str() == "content-box" => ResizeBox::ContentBox,
                Value::Str(value) if value.as_str() == "border-box" => ResizeBox::BorderBox,
                Value::Str(value) if value.as_str() == "device-pixel-content-box" => {
                    ResizeBox::DevicePixelContentBox
                }
                _ => {
                    return Err(OpError::new(
                        "TypeError",
                        "invalid ResizeObserver box option",
                    ));
                }
            }
        } else {
            ResizeBox::ContentBox
        };
        let mut targets = self.data.targets.borrow_mut();
        match targets.get_mut(&target.id) {
            Some(existing) if existing.box_kind != box_kind => {
                existing.state.reset();
                existing.box_kind = box_kind;
            }
            Some(_) => (),
            None => {
                targets.insert(
                    target.id,
                    ResizeTarget {
                        state: ResizeState::new(),
                        box_kind,
                        _keep: DomNodeList::snapshot(
                            realm.clone(),
                            vec![target.id],
                            Value::Undefined,
                        ),
                    },
                );
            }
        }
        drop(targets);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        ctx.retain_instance(&this.0, true);
        Ok(())
    }
    fn unobserve(&self, target: &DomNode) {
        self.data.targets.borrow_mut().remove(&target.id);
    }
    fn disconnect(&self, ctx: &mut Ctx, this: This<Value>) {
        self.data.targets.borrow_mut().clear();
        self.data.pending.borrow_mut().clear();
        ctx.retain_instance(&this.0, false);
    }
    fn take_records(&self, ctx: &mut Ctx) -> OpResult<Vec<Value>> {
        let realm = self
            .hub
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        let pending = std::mem::take(&mut *self.data.pending.borrow_mut());
        pending
            .into_iter()
            .map(|item| resize_entry(ctx, &realm, item.entry, item.target))
            .collect()
    }
}

#[lumen_bind::class(name = "IntersectionObserver", hint(js(webidl)))]
pub struct DomIntersectionObserver {
    hub: Rc<IntersectionHub>,
    data: Rc<IntersectionData>,
}
#[lumen_bind::methods]
impl DomIntersectionObserver {
    #[constructor]
    fn new(ctx: &mut Ctx, callback: JsFunction, options: Option<Value>) -> OpResult<Self> {
        let hub = RealmServices::<IntersectionHub>::current(ctx)
            .ok_or_else(|| OpError::new("Error", "IntersectionObserver is not installed"))?;
        let mut thresholds = Vec::new();
        let mut root = None;
        let mut root_margin = [Margin::default(); 4];
        let mut root_margin_text = "0px 0px 0px 0px".to_owned();
        if let Some(options) = options {
            let root_value = ctx
                .get_member(&options, "root")
                .map_err(|_| OpError::new("TypeError", "root option getter failed"))?;
            if !matches!(root_value, Value::Undefined | Value::Null) {
                let (root_realm, root_node) = ctx
                    .with_instance::<DomNode, _>(&root_value, |node| (node.realm.clone(), node.id))
                    .map_err(|_| OpError::new("TypeError", "root must be an Element or null"))?;
                let realm = hub
                    .realm
                    .upgrade()
                    .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
                if !Rc::ptr_eq(&realm, &root_realm) {
                    return Err(OpError::new(
                        "WrongDocumentError",
                        "root belongs to another realm",
                    ));
                }
                if !matches!(
                    realm.session.borrow().document().kind(root_node),
                    Ok(lumen_html::NodeKind::Element { .. } | lumen_html::NodeKind::Document)
                ) {
                    return Err(OpError::new(
                        "TypeError",
                        "root must be an Element, Document, or null",
                    ));
                }
                root = Some(root_node);
            }
            let margin_value = ctx
                .get_member(&options, "rootMargin")
                .map_err(|_| OpError::new("TypeError", "rootMargin option getter failed"))?;
            if !matches!(margin_value, Value::Undefined) {
                let raw = ctx
                    .coerce_string(&margin_value)
                    .map_err(OpError::thrown)?
                    .to_string();
                (root_margin, root_margin_text) = parse_root_margin(&raw)
                    .map_err(|message| OpError::new("SyntaxError", message))?;
            }
            let value = ctx
                .get_member(&options, "threshold")
                .map_err(|_| OpError::new("TypeError", "threshold option getter failed"))?;
            match value {
                Value::Undefined => (),
                Value::Num(value) => thresholds.push(value),
                value if ctx.is_array_value(&value).map_err(OpError::thrown)? => {
                    let Value::Num(length) = ctx
                        .get_member(&value, "length")
                        .map_err(|_| OpError::new("TypeError", "threshold length failed"))?
                    else {
                        return Err(OpError::new("TypeError", "invalid thresholds"));
                    };
                    if length > 1000.0 {
                        return Err(OpError::new("RangeError", "too many thresholds"));
                    }
                    for index in 0..length as usize {
                        let item = ctx
                            .get_member(&value, &index.to_string())
                            .map_err(|_| OpError::new("TypeError", "threshold read failed"))?;
                        let Value::Num(number) = item else {
                            return Err(OpError::new("TypeError", "threshold must be numeric"));
                        };
                        thresholds.push(number);
                    }
                }
                _ => {
                    return Err(OpError::new(
                        "TypeError",
                        "threshold must be a number or array",
                    ));
                }
            }
        }
        let state = IntersectionState::new(thresholds)
            .map_err(|message| OpError::new("RangeError", message))?;
        let data = Rc::new(IntersectionData {
            callback,
            thresholds: state.thresholds,
            root,
            root_margin,
            root_margin_text,
            targets: RefCell::new(HashMap::new()),
            pending: RefCell::new(Vec::new()),
            wrapper: RefCell::new(None),
        });
        hub.observers.borrow_mut().push(Rc::downgrade(&data));
        Ok(Self { hub, data })
    }
    #[getter]
    fn root(&self, ctx: &mut Ctx) -> Value {
        self.data.root.map_or(Value::Null, |root| {
            self.hub
                .realm
                .upgrade()
                .map_or(Value::Null, |realm| realm.wrap(ctx, root))
        })
    }
    #[getter(name = "rootMargin")]
    fn root_margin(&self) -> String {
        self.data.root_margin_text.clone()
    }
    #[getter]
    fn thresholds(&self, ctx: &mut Ctx) -> Value {
        JsHost::from_list(
            ctx,
            self.data
                .thresholds
                .iter()
                .copied()
                .map(Value::Num)
                .collect(),
        )
    }
    fn observe(&self, ctx: &mut Ctx, this: This<Value>, target: &DomNode) -> OpResult<()> {
        let realm = self
            .hub
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        if !Rc::ptr_eq(&realm, &target.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "observer target belongs to another realm",
            ));
        }
        if !matches!(
            realm.session.borrow().document().kind(target.id),
            Ok(lumen_html::NodeKind::Element { .. })
        ) {
            return Err(OpError::new(
                "TypeError",
                "IntersectionObserver target must be an Element",
            ));
        }
        self.data
            .targets
            .borrow_mut()
            .entry(target.id)
            .or_insert_with(|| IntersectionTarget {
                state: IntersectionState::new(self.data.thresholds.clone())
                    .expect("validated thresholds"),
                _keep: DomNodeList::snapshot(realm, vec![target.id], Value::Undefined),
            });
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        ctx.retain_instance(&this.0, true);
        Ok(())
    }
    fn unobserve(&self, target: &DomNode) {
        self.data.targets.borrow_mut().remove(&target.id);
    }
    fn disconnect(&self, ctx: &mut Ctx, this: This<Value>) {
        self.data.targets.borrow_mut().clear();
        self.data.pending.borrow_mut().clear();
        ctx.retain_instance(&this.0, false);
    }
    fn take_records(&self, ctx: &mut Ctx) -> OpResult<Vec<Value>> {
        let realm = self
            .hub
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        let pending = std::mem::take(&mut *self.data.pending.borrow_mut());
        pending
            .into_iter()
            .map(|item| intersection_entry(ctx, &realm, item.entry, item.target))
            .collect()
    }
}

fn set(ctx: &mut Ctx, object: &Value, name: &str, value: Value) -> OpResult<()> {
    ctx.set_member(object, name, value)
        .map_err(|_| OpError::new("TypeError", "observer entry initialization failed"))
}

fn size_value(ctx: &mut Ctx, inline: f32, block: f32) -> Value {
    let value = Value::Obj(ctx.new_object());
    let _ = set(ctx, &value, "inlineSize", Value::Num(f64::from(inline)));
    let _ = set(ctx, &value, "blockSize", Value::Num(f64::from(block)));
    value
}

fn resize_entry(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    entry: ResizeEntry,
    target: NodeId,
) -> OpResult<Value> {
    let value = Value::Obj(ctx.new_object());
    let target_value = realm.wrap(ctx, target);
    set(ctx, &value, "target", target_value)?;
    let content_rect = crate::geometry::readonly_rect_value(ctx, entry.content_rect);
    set(ctx, &value, "contentRect", content_rect)?;
    let content_size = size_value(ctx, entry.content_box_size.0, entry.content_box_size.1);
    let content_box = JsHost::from_list(ctx, vec![content_size]);
    set(ctx, &value, "contentBoxSize", content_box)?;
    let border_size = size_value(ctx, entry.border_box_size.0, entry.border_box_size.1);
    let border_box = JsHost::from_list(ctx, vec![border_size]);
    set(ctx, &value, "borderBoxSize", border_box)?;
    let device_size = size_value(
        ctx,
        entry.device_pixel_content_box_size.0,
        entry.device_pixel_content_box_size.1,
    );
    let device_box = JsHost::from_list(ctx, vec![device_size]);
    set(ctx, &value, "devicePixelContentBoxSize", device_box)?;
    Ok(value)
}

fn intersection_entry(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    entry: IntersectionEntry,
    target: NodeId,
) -> OpResult<Value> {
    let value = Value::Obj(ctx.new_object());
    let target_value = realm.wrap(ctx, target);
    set(ctx, &value, "target", target_value)?;
    let bounds = crate::geometry::readonly_rect_value(ctx, entry.bounding_client_rect);
    set(ctx, &value, "boundingClientRect", bounds)?;
    let root_bounds = crate::geometry::readonly_rect_value(ctx, entry.root_bounds);
    set(ctx, &value, "rootBounds", root_bounds)?;
    let intersection = crate::geometry::readonly_rect_value(ctx, entry.intersection_rect);
    set(ctx, &value, "intersectionRect", intersection)?;
    set(
        ctx,
        &value,
        "intersectionRatio",
        Value::Num(entry.intersection_ratio),
    )?;
    set(
        ctx,
        &value,
        "isIntersecting",
        Value::Bool(entry.is_intersecting),
    )?;
    Ok(value)
}

#[lumen_bind::class(name = "ResizeObserverDelivery")]
struct ResizeDelivery {
    hub: Rc<ResizeHub>,
}
#[lumen_bind::methods]
impl ResizeDelivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.hub.scheduled.set(false);
        let Some(realm) = self.hub.realm.upgrade() else {
            return Ok(());
        };
        let observers = self
            .hub
            .observers
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let mut failure = None;
        for observer in observers {
            let pending = std::mem::take(&mut *observer.pending.borrow_mut());
            if pending.is_empty() {
                continue;
            }
            let entries = pending
                .into_iter()
                .map(|item| resize_entry(ctx, &realm, item.entry, item.target))
                .collect::<OpResult<Vec<_>>>()?;
            let entries = JsHost::from_list(ctx, entries);
            let wrapper = observer
                .wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
                .unwrap_or(Value::Undefined);
            if let Err(error) = observer
                .callback
                .call(ctx, wrapper.clone(), &[entries, wrapper])
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

#[lumen_bind::class(name = "IntersectionObserverDelivery")]
struct IntersectionDelivery {
    hub: Rc<IntersectionHub>,
}
#[lumen_bind::methods]
impl IntersectionDelivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.hub.scheduled.set(false);
        let Some(realm) = self.hub.realm.upgrade() else {
            return Ok(());
        };
        let observers = self
            .hub
            .observers
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let mut failure = None;
        for observer in observers {
            let pending = std::mem::take(&mut *observer.pending.borrow_mut());
            if pending.is_empty() {
                continue;
            }
            let entries = pending
                .into_iter()
                .map(|item| intersection_entry(ctx, &realm, item.entry, item.target))
                .collect::<OpResult<Vec<_>>>()?;
            let entries = JsHost::from_list(ctx, entries);
            let wrapper = observer
                .wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
                .unwrap_or(Value::Undefined);
            if let Err(error) = observer
                .callback
                .call(ctx, wrapper.clone(), &[entries, wrapper])
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn bind_delivery(ctx: &mut Ctx, delivery: Value, value: Value) -> OpResult<Value> {
    let flush = ctx
        .get_member(&delivery, "flush")
        .map_err(|_| OpError::new("Error", "observer delivery missing"))?;
    let bind = ctx
        .get_member(&flush, "bind")
        .map_err(|_| OpError::new("Error", "observer delivery bind missing"))?;
    let function = JsFunction::from_value(bind)
        .ok_or_else(|| OpError::new("TypeError", "observer delivery binding invalid"))?;
    function.call(ctx, flush, &[value])
}

/// Called by the host only after `display_list*` succeeds. It samples the
/// exact retained frame; the host controls ResizeObserver pre-paint and
/// IntersectionObserver task delivery through the phase functions below.
pub fn layout_completed(ctx: &mut Ctx) {
    let Some(resize_hub) = RealmServices::<ResizeHub>::current(ctx) else {
        return;
    };
    let Some(intersection_hub) = RealmServices::<IntersectionHub>::current(ctx) else {
        return;
    };
    let Some(realm) = resize_hub.realm.upgrade() else {
        return;
    };
    let resize_observers = resize_hub
        .observers
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for observer in resize_observers {
        let mut targets = observer.targets.borrow_mut();
        for (target, observation) in targets.iter_mut() {
            let sampled = {
                let mut session = realm.session.borrow_mut();
                let geometry = crate::geometry::snapshot(&mut session, *target);
                geometry.and_then(|geometry| {
                    let resolution = session.media_environment().resolution;
                    let content_box_size =
                        (geometry.content_rect.width, geometry.content_rect.height);
                    let device_pixel_size = (
                        content_box_size.0 * resolution,
                        content_box_size.1 * resolution,
                    );
                    let observed_size = match observation.box_kind {
                        ResizeBox::ContentBox => content_box_size,
                        ResizeBox::BorderBox => geometry.border_box_size,
                        ResizeBox::DevicePixelContentBox => device_pixel_size,
                    };
                    observation.state.sample_with_box(
                        *target,
                        geometry.content_rect,
                        geometry.border_box_size,
                        observed_size,
                        device_pixel_size,
                    )
                })
            };
            if let Some(entry) = sampled {
                observer.pending.borrow_mut().push(ResizePending {
                    target: *target,
                    entry,
                    _keep: DomNodeList::snapshot(realm.clone(), vec![*target], Value::Undefined),
                });
            }
        }
        if !observer.pending.borrow().is_empty() {
            resize_hub.schedule();
        }
    }
    let intersection_observers = intersection_hub
        .observers
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for observer in intersection_observers {
        let mut targets = observer.targets.borrow_mut();
        for (target, observation) in targets.iter_mut() {
            let sampled = sample_observer_intersection(
                &mut realm.session.borrow_mut(),
                &mut observation.state,
                *target,
                observer.root,
                observer.root_margin,
            );
            if let Some(entry) = sampled {
                observer.pending.borrow_mut().push(IntersectionPending {
                    target: *target,
                    entry,
                    _keep: DomNodeList::snapshot(realm.clone(), vec![*target], Value::Undefined),
                });
            }
        }
        if !observer.pending.borrow().is_empty() {
            intersection_hub.schedule();
        }
    }
}

/// Deliver pending ResizeObserver callbacks in the host's pre-paint phase.
pub fn deliver_resize(ctx: &mut Ctx) -> OpResult<()> {
    let Some(hub) = RealmServices::<ResizeHub>::current(ctx) else {
        return Ok(());
    };
    if !hub.scheduled.replace(false) {
        return Ok(());
    }
    let callback = hub.delivery.borrow().clone();
    if let Some(callback) = callback {
        callback.call(ctx, Value::Undefined, &[])?;
    }
    Ok(())
}

/// True when the host should enqueue one IntersectionObserver task.
pub fn intersection_pending(ctx: &mut Ctx) -> bool {
    RealmServices::<IntersectionHub>::current(ctx).is_some_and(|hub| hub.scheduled.get())
}

/// Deliver IntersectionObserver callbacks from the host's task queue.
pub fn deliver_intersections(ctx: &mut Ctx) -> OpResult<()> {
    let Some(hub) = RealmServices::<IntersectionHub>::current(ctx) else {
        return Ok(());
    };
    if !hub.scheduled.replace(false) {
        return Ok(());
    }
    let callback = hub.delivery.borrow().clone();
    if let Some(callback) = callback {
        callback.call(ctx, Value::Undefined, &[])?;
    }
    Ok(())
}

pub fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let resize_hub = RealmServices::replace_current(
        ctx,
        ResizeHub {
            realm: Rc::downgrade(realm),
            observers: RefCell::new(Vec::new()),
            delivery: RefCell::new(None),
            scheduled: Cell::new(false),
        },
    );
    let intersection_hub = RealmServices::replace_current(
        ctx,
        IntersectionHub {
            realm: Rc::downgrade(realm),
            observers: RefCell::new(Vec::new()),
            delivery: RefCell::new(None),
            scheduled: Cell::new(false),
        },
    );
    ctx.class_constructor::<ResizeDelivery>();
    ctx.class_constructor::<IntersectionDelivery>();
    let resize_delivery = ctx.new_instance(ResizeDelivery {
        hub: resize_hub.clone(),
    });
    let resize_bound = bind_delivery(ctx, resize_delivery.clone(), resize_delivery)?;
    *resize_hub.delivery.borrow_mut() = Some(
        JsFunction::from_value(resize_bound)
            .ok_or_else(|| OpError::new("TypeError", "ResizeObserver delivery invalid"))?,
    );
    let intersection_delivery = ctx.new_instance(IntersectionDelivery {
        hub: intersection_hub.clone(),
    });
    let intersection_bound =
        bind_delivery(ctx, intersection_delivery.clone(), intersection_delivery)?;
    *intersection_hub.delivery.borrow_mut() = Some(
        JsFunction::from_value(intersection_bound)
            .ok_or_else(|| OpError::new("TypeError", "IntersectionObserver delivery invalid"))?,
    );
    let global = ctx.global_object();
    let resize_constructor = ctx.class_constructor::<DomResizeObserver>();
    let intersection_constructor = ctx.class_constructor::<DomIntersectionObserver>();
    crate::install_interface(ctx, &global, "ResizeObserver", resize_constructor)
        .map_err(|_| OpError::new("Error", "ResizeObserver installation failed"))?;
    crate::install_interface(
        ctx,
        &global,
        "IntersectionObserver",
        intersection_constructor,
    )
    .map_err(|_| OpError::new("Error", "IntersectionObserver installation failed"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_html::{
        html,
        paint::{ShapedRun, TextShaper},
        session::RenderSession,
    };
    use lumen_html_text::{FontFace, DEFAULT_FONT_BYTES};
    use std::sync::Arc;

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

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let detail = engine
                    .ctx()
                    .get_member(&error, "stack")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(text) => Some(text.as_str().to_owned()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "JavaScript exception without a stack".into());
                panic!("JavaScript evaluation threw: {detail}\nSource: {source}");
            }
        }
    }

    fn install_with_font(source: &str) -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), source, 128).unwrap();
        let font = Rc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let retained_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(120, 100, retained_font.as_ref())
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        (engine, realm)
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn resize_samples_initial_and_actual_size_changes_only() {
        let document = lumen_html::html::parse("<div></div>", 8).unwrap();
        let target = lumen_html::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let mut state = ResizeState::new();
        let first = state
            .sample(target, rect(4.0, 6.0, 20.0, 10.0), (24.0, 14.0))
            .unwrap();
        assert_eq!(first.content_rect.width, 20.0);
        assert!(state
            .sample(target, rect(4.0, 6.0, 20.0, 10.0), (24.0, 14.0))
            .is_none());
        assert_eq!(
            state
                .sample(target, rect(4.0, 6.0, 30.0, 10.0), (34.0, 14.0))
                .unwrap()
                .border_box_size,
            (34.0, 14.0)
        );
    }

    #[test]
    fn intersection_observes_initial_state_and_threshold_crossings() {
        let document = lumen_html::html::parse("<div></div>", 8).unwrap();
        let target = lumen_html::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let mut state = IntersectionState::new(vec![0.0, 0.5, 1.0]).unwrap();
        let first = state
            .sample(
                target,
                rect(5.0, 0.0, 10.0, 10.0),
                rect(0.0, 0.0, 10.0, 10.0),
            )
            .unwrap();
        assert_eq!(first.intersection_ratio, 0.5);
        assert!(first.is_intersecting);
        assert!(state
            .sample(
                target,
                rect(5.0, 0.0, 10.0, 10.0),
                rect(0.0, 0.0, 10.0, 10.0)
            )
            .is_none());
        let next = state
            .sample(
                target,
                rect(10.0, 0.0, 10.0, 10.0),
                rect(0.0, 0.0, 10.0, 10.0),
            )
            .unwrap();
        assert_eq!(next.intersection_ratio, 0.0);
        // The boxes are edge-adjacent. IntersectionObserver defines this as
        // intersecting even though the intersection area and ratio are zero.
        assert!(next.is_intersecting);
    }

    #[test]
    fn intersection_thresholds_are_validated_sorted_and_deduplicated() {
        assert!(IntersectionState::new(vec![-0.1]).is_err());
        let state = IntersectionState::new(vec![1.0, 0.5, 0.5]).unwrap();
        assert_eq!(state.thresholds, vec![0.5, 1.0]);
    }

    #[test]
    fn observers_sample_real_retained_layout_and_overflow_clipping() {
        let document = html::parse("<style>body{margin:0}#clip{width:30px;height:20px;overflow:hidden}#target{width:20px;height:10px;transform:translate(25px,0)}</style><div id=clip><div id=target></div></div>", 32).unwrap();
        let clip = lumen_html::selector::query_selector(&document, document.root(), "#clip")
            .unwrap()
            .unwrap();
        let target = lumen_html::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();

        let mut resize = ResizeState::new();
        let first_resize = sample_resize(&mut session, &mut resize, target).unwrap();
        assert_eq!(first_resize.content_rect.width, 20.0);
        assert!(sample_resize(&mut session, &mut resize, target).is_none());

        let root_rect = session.hit_bounds(clip).unwrap();
        let mut intersection = IntersectionState::new(vec![0.0, 0.25, 0.5]).unwrap();
        let first_intersection =
            sample_intersection(&mut session, &mut intersection, target, root_rect).unwrap();
        assert_eq!(first_intersection.intersection_ratio, 0.25);
        assert!(first_intersection.is_intersecting);

        session
            .document_mut()
            .set_attribute(
                target,
                "style",
                "width:40px;height:10px;transform:translate(25px,0)",
            )
            .unwrap();
        session.display_list(100, 100, &NoText).unwrap();
        let resized = sample_resize(&mut session, &mut resize, target).unwrap();
        assert_eq!(resized.content_rect.width, 40.0);
        let next_intersection =
            sample_intersection(&mut session, &mut intersection, target, root_rect).unwrap();
        assert_eq!(next_intersection.intersection_ratio, 0.125);
        assert!(next_intersection.is_intersecting);
    }

    #[test]
    fn root_margin_expands_viewport_edges_and_parses_shorthand() {
        let (margin, normalized) = parse_root_margin("10% 2px").unwrap();
        assert_eq!(normalized, "10% 2px 10% 2px");
        let expanded = expand_root(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0,
            },
            margin,
        );
        assert_eq!(
            expanded,
            Rect {
                x: -2.0,
                y: -10.0,
                width: 104.0,
                height: 70.0
            }
        );
        assert!(parse_root_margin("2em").is_err());
    }

    #[test]
    fn javascript_geometry_flushes_layout_and_returns_dom_rect_types() {
        let (mut engine, _) = install_with_font(
            "<style>body{margin:0}#target{width:20px;height:10px;transform:translate(5px,3px) scale(2);transform-origin:0 0}</style><div id=target></div>",
        );
        let initial_geometry = eval(
            &mut engine,
            "var target=document.querySelector('#target'); var rect=target.getBoundingClientRect(); var list=target.getClientRects(); rect instanceof DOMRect && list instanceof DOMRectList && list.length===1 && list.item(0) instanceof DOMRect && rect.x===5 && rect.y===3 && rect.width===40 && target.offsetWidth===20 && target.clientWidth===20",
        );
        let geometry_diagnostics = match eval(
            &mut engine,
            "JSON.stringify({rectType:rect instanceof DOMRect, listType:list instanceof DOMRectList, listLength:list.length, itemType:list.item(0) instanceof DOMRect, rect:rect.toJSON(), offsetWidth:target.offsetWidth, clientWidth:target.clientWidth, checks:[rect.x===5,rect.y===3,rect.width===40,list.length===1,target.offsetWidth===20,target.clientWidth===20]})",
        ) {
            Value::Str(value) => value.as_str().to_owned(),
            _ => "JSON.stringify did not return a string".to_owned(),
        };
        let initial_geometry_description = match &initial_geometry {
            Value::Bool(value) => format!("Bool({value})"),
            Value::Num(value) => format!("Num({value})"),
            Value::Str(value) => format!("Str({})", value.as_str()),
            Value::Undefined => "Undefined".to_owned(),
            Value::Empty => "Empty".to_owned(),
            Value::Null => "Null".to_owned(),
            Value::BigInt(_) => "BigInt".to_owned(),
            Value::Sym(_) => "Symbol".to_owned(),
            Value::Obj(_) => "Object".to_owned(),
        };
        assert!(
            matches!(initial_geometry, Value::Bool(true)),
            "initial geometry result: {initial_geometry_description}; geometry API values: {geometry_diagnostics}"
        );
        assert!(matches!(
            eval(
                &mut engine,
                "target.setAttribute('style','width:30px;height:10px;transform:translate(5px,3px) scale(2);transform-origin:0 0'); target.getBoundingClientRect().width===60 && target.offsetWidth===30"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(
                &mut engine,
                "var writableRect=new DOMRect(1,2,3,4); writableRect.x=5; writableRect.y=6; writableRect.width=7; writableRect.height=8; writableRect.toJSON().x===5 && writableRect.toJSON().y===6 && writableRect.toJSON().width===7 && writableRect.toJSON().height===8"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn resize_uses_prepaint_delivery_and_intersection_uses_host_task() {
        let (mut engine, realm) = install_with_font(
            "<style>body{margin:0}#target{width:10px;height:10px;transform:translate(125px,0)}</style><div id=target></div>",
        );
        eval(
            &mut engine,
            "var target=document.querySelector('#target'); var resizeEntries=[]; var intersectionEntries=[]; var ro=new ResizeObserver(entries=>resizeEntries.push(...entries)); ro.observe(target); var io=new IntersectionObserver(entries=>intersectionEntries.push(...entries),{root:null,rootMargin:'0px 10px',threshold:[0,0.5,1]}); io.observe(target);",
        );
        realm.flush_layout().unwrap();
        layout_completed(engine.ctx());
        assert!(matches!(
            eval(
                &mut engine,
                "resizeEntries.length===0 && intersectionEntries.length===0"
            ),
            Value::Bool(true)
        ));
        assert!(intersection_pending(engine.ctx()));
        deliver_resize(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "resizeEntries.length===1 && resizeEntries[0].target===target && resizeEntries[0].contentRect.width===10 && resizeEntries[0].borderBoxSize[0].inlineSize===10"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(&mut engine, "intersectionEntries.length===0"),
            Value::Bool(true)
        ));
        deliver_intersections(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "intersectionEntries.length===1 && intersectionEntries[0].target===target && intersectionEntries[0].intersectionRatio===0.5 && intersectionEntries[0].isIntersecting && intersectionEntries[0].boundingClientRect instanceof DOMRectReadOnly && io.root===null && io.rootMargin==='0px 10px 0px 10px' && io.thresholds.length===3"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn observer_hubs_deliver_only_in_their_active_host_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let source =
            "<style>body{margin:0}#target{width:10px;height:10px}</style><div id=target></div>";
        let parent = crate::install(ctx, source, 64).unwrap();
        let parent_global = ctx.global_object();
        let parent_font = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let retained_parent_font = parent_font.clone();
        parent.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(120, 100, retained_parent_font.as_ref())
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "window.__observerCounts={resize:0,intersection:0}; \
                 window.__target=document.getElementById('target'); \
                 window.__resizeObserver=new ResizeObserver(()=>__observerCounts.resize++); \
                 __resizeObserver.observe(__target); \
                 window.__intersectionObserver=new IntersectionObserver(()=>__observerCounts.intersection++); \
                 __intersectionObserver.observe(__target);",
            ),
            Ok(Value::Undefined)
        ));

        let child_handle = ctx.create_host_realm();
        let child_global = child_handle.global();
        let child = ctx
            .with_host_realm(&child_handle, |ctx| {
                let realm = crate::install(ctx, source, 64).unwrap();
                let child_font = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
                let retained_child_font = child_font.clone();
                realm.set_layout_flusher(Rc::new(move |session| {
                    session
                        .display_list(120, 100, retained_child_font.as_ref())
                        .map(|_| ())
                        .map_err(|error| format!("{error:?}"))
                }));
                assert!(matches!(
                    ctx.eval_in_realm(
                        &child_global,
                        "window.__observerCounts={resize:0,intersection:0}; \
                         window.__target=document.getElementById('target'); \
                         window.__resizeObserver=new ResizeObserver(()=>__observerCounts.resize++); \
                         __resizeObserver.observe(__target); \
                         window.__intersectionObserver=new IntersectionObserver(()=>__observerCounts.intersection++); \
                         __intersectionObserver.observe(__target);",
                    ),
                    Ok(Value::Undefined)
                ));
                realm
            })
            .expect("install child observer realm");

        parent.flush_layout().unwrap();
        child.flush_layout().unwrap();

        // The child layout sample and both native delivery phases must only touch
        // the observers registered by the child global.
        ctx.with_host_realm(&child_handle, |ctx| {
            layout_completed(ctx);
            assert!(intersection_pending(ctx));
            deliver_resize(ctx).unwrap();
            deliver_intersections(ctx).unwrap();
        })
        .expect("deliver observer notifications in the child realm");
        assert!(matches!(
            ctx.eval_in_realm(
                &child_global,
                "__observerCounts.resize===1 && __observerCounts.intersection===1",
            ),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "__observerCounts.resize===0 && __observerCounts.intersection===0",
            ),
            Ok(Value::Bool(true))
        ));

        layout_completed(ctx);
        assert!(intersection_pending(ctx));
        deliver_resize(ctx).unwrap();
        deliver_intersections(ctx).unwrap();
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "__observerCounts.resize===1 && __observerCounts.intersection===1",
            ),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(
                &child_global,
                "__observerCounts.resize===1 && __observerCounts.intersection===1",
            ),
            Ok(Value::Bool(true))
        ));
    }

    #[test]
    fn explicit_root_clips_and_detached_targets_report_zero_geometry() {
        let (mut engine, realm) = install_with_font(
            "<style>body{margin:0}#root{width:30px;height:20px;overflow:hidden}#target{width:20px;height:10px;transform:translate(25px,0)}</style><div id=root><div id=target></div></div>",
        );
        eval(
            &mut engine,
            "var root=document.querySelector('#root'); var target=document.querySelector('#target'); var roEntries=[]; var ioEntries=[]; var ro=new ResizeObserver(entries=>roEntries.push(...entries)); ro.observe(target,{box:'border-box'}); var io=new IntersectionObserver(entries=>ioEntries.push(...entries),{root,rootMargin:'0px',threshold:[0,0.25,1]}); io.observe(target);",
        );
        realm.flush_layout().unwrap();
        layout_completed(engine.ctx());
        deliver_resize(engine.ctx()).unwrap();
        assert!(intersection_pending(engine.ctx()));
        deliver_intersections(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "io.root===root && io.rootMargin==='0px 0px 0px 0px' && io.thresholds.length===3 && ioEntries.length===1 && ioEntries[0].intersectionRatio===0.25 && ioEntries[0].isIntersecting"
            ),
            Value::Bool(true)
        ));

        eval(&mut engine, "root.removeChild(target)");
        realm.flush_layout().unwrap();
        layout_completed(engine.ctx());
        assert!(intersection_pending(engine.ctx()));
        deliver_resize(engine.ctx()).unwrap();
        deliver_intersections(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "roEntries.length===2 && roEntries[1].borderBoxSize[0].inlineSize===0 && ioEntries.length===2 && ioEntries[1].intersectionRatio===0 && !ioEntries[1].isIntersecting"
            ),
            Value::Bool(true)
        ));
    }
}
