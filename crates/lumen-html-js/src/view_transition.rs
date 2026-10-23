//! View-transition promises belong to the native transition, never to an
//! author-replaced Promise constructor or `then` method.
use super::*;
use lumen::embed::JsFunction;
use lumen::embed::{Deferred, NativeIdentityOwner};
use lumen_bind::This;
use lumen_html::render_capture::ReservedImageData;
use lumen_html::{
    animation::Keyframe,
    css::{self, PseudoElement},
    paint::{Command, DisplayList, Rect},
};
use std::{rc::Weak, sync::Arc};

/// Present two genuine captures through the maintained native canvas rasterizer.
/// The output and scratch reservations are admitted before allocating any
/// bitmap. Scratch covers the premultiplied surface and one image conversion;
/// the retained straight-RGBA output carries its own lease afterwards.
fn composite_captures(
    old: &Arc<ReservedImageData>,
    new: &Arc<ReservedImageData>,
    old_opacity: f64,
    new_opacity: f64,
    budget: &Arc<lumen_common::limits::ByteBudget>,
) -> OpResult<Arc<ReservedImageData>> {
    if (old.image.width, old.image.height) != (new.image.width, new.image.height) {
        return Err(OpError::new(
            "InvalidStateError",
            "View transition viewport dimensions changed",
        ));
    }
    let request = RenderCaptureRequest {
        target: RenderCaptureTarget::Viewport,
        width: old.image.width,
        height: old.image.height,
    };
    let output = request.reserve(budget).ok_or_else(|| {
        OpError::new(
            "QuotaExceededError",
            "View transition output exceeds the shared pixel budget",
        )
    })?;
    let scratch = budget
        .reserve(output.bytes().checked_mul(2).ok_or_else(|| {
            OpError::new(
                "QuotaExceededError",
                "View transition scratch size overflow",
            )
        })?)
        .ok_or_else(|| {
            OpError::new(
                "QuotaExceededError",
                "View transition scratch exceeds the shared pixel budget",
            )
        })?;
    let mut surface = lumen_html_image::canvas::CanvasSurface::new(request.width, request.height)
        .map_err(|error| {
        OpError::new(
            "InvalidStateError",
            format!("View transition surface: {error:?}"),
        )
    })?;
    surface.state_mut().blend = tiny_skia::BlendMode::Plus;
    for (image, opacity) in [(old, old_opacity), (new, new_opacity)] {
        surface.state_mut().alpha = opacity.clamp(0.0, 1.0) as f32;
        surface
            .draw_image(
                &image.image,
                0.0,
                0.0,
                request.width as f32,
                request.height as f32,
            )
            .map_err(|error| {
                OpError::new(
                    "InvalidStateError",
                    format!("View transition image: {error:?}"),
                )
            })?;
    }
    let image = surface.snapshot();
    drop(surface);
    drop(scratch);
    ReservedImageData::new(image, output).ok_or_else(|| {
        OpError::new(
            "InvalidStateError",
            "View transition produced invalid pixel storage",
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    PendingOldCapture,
    Updating,
    PendingNewCapture,
    Animating,
    Skipped,
    Done,
}

struct Settlement {
    promise: Value,
    deferred: Option<Deferred>,
}
impl Default for Settlement {
    fn default() -> Self {
        Self {
            promise: Value::Undefined,
            deferred: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    Group,
    FadeOld,
    FadeNew,
    Blend,
}

#[derive(Clone)]
struct CapturedElement {
    name: Arc<str>,
    node: NodeId,
    rect: [f32; 4],
    style: Arc<css::Style>,
}

/// One generated capture group of the transition being presented, as seen by
/// computed-style queries on its pseudo-elements.
#[derive(Clone)]
pub(crate) struct ActiveCapture {
    name: Arc<str>,
    old: bool,
    new: bool,
    source: Arc<css::Style>,
}

struct TransitionState {
    owner: Option<Weak<DomRealm>>,
    scope_root: Option<NodeId>,
    captures: Vec<CapturedElement>,
    roles: Vec<Role>,
    types: Rc<RefCell<lumen::embed::DomStringSet>>,
    types_wrapper: Option<Value>,
    wait_pending: usize,
    new_frame_id: Option<u64>,
    queued: bool,
    queued_next: Option<Value>,
    animations: Vec<(u32, Value, bool, f64)>,
    opacity: [f64; 2],
    phase: Phase,
    callback: Option<Value>,
    callback_invoked: bool,
    callback_done: bool,
    ready: Settlement,
    update_done: Settlement,
    finished: Settlement,
    old: Option<Arc<ReservedImageData>>,
    new: Option<Arc<ReservedImageData>>,
}

#[lumen_bind::class(name = "ViewTransition", hint(js(webidl)))]
pub struct DomViewTransition {
    state: RefCell<TransitionState>,
}

impl NativeIdentityOwner for DomViewTransition {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Ok(state) = self.state.try_borrow() {
            if let Some(callback) = &state.callback {
                visit(callback);
            }
            if let Some(next) = &state.queued_next {
                visit(next);
            }
            for (_, wrapper, _, _) in &state.animations {
                visit(wrapper);
            }
            if let Some(types) = &state.types_wrapper {
                visit(types);
            }
            for settlement in [&state.ready, &state.update_done, &state.finished] {
                visit(&settlement.promise);
                // Deferred and public promise are two actual stored edges.
                if let Some(deferred) = &settlement.deferred {
                    visit(deferred.promise_value());
                }
            }
        }
    }
}

#[lumen_bind::methods]
impl DomViewTransition {
    #[getter]
    fn ready(ctx: &mut Ctx, this: This<Value>) -> Value {
        promise_attribute(ctx, &this.0, |state| state.ready.promise.clone())
    }
    #[getter]
    fn update_callback_done(ctx: &mut Ctx, this: This<Value>) -> Value {
        promise_attribute(ctx, &this.0, |state| state.update_done.promise.clone())
    }
    #[getter]
    fn finished(ctx: &mut Ctx, this: This<Value>) -> Value {
        promise_attribute(ctx, &this.0, |state| state.finished.promise.clone())
    }

    fn skip_transition(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        skip(ctx, &this.0, "AbortError", "View transition skipped")
    }

    #[getter]
    fn types(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if let Some(wrapper) = self.state.borrow().types_wrapper.clone() {
            return Ok(wrapper);
        }
        let data = self.state.borrow().types.clone();
        let wrapper = ctx.new_instance(DomViewTransitionTypeSet { data });
        self.state.borrow_mut().types_wrapper = Some(wrapper.clone());
        Ok(wrapper)
    }

    #[getter]
    fn transition_root(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (document, scope) = {
            let state = self.state.borrow();
            (state.owner.as_ref().and_then(Weak::upgrade), state.scope_root)
        };
        let Some(document) = document else {
            return Ok(Value::Null);
        };
        let root = match scope {
            Some(node) => node,
            None => viewport_origin(&document)?,
        };
        Ok(document.wrap_option(ctx, Some(root)))
    }

    fn wait_until(&self, ctx: &mut Ctx, this: This<Value>, promise: Value) -> OpResult<()> {
        {
            let mut state = self.state.borrow_mut();
            if matches!(state.phase, Phase::Done | Phase::Skipped) {
                return Ok(());
            }
            state.wait_pending += 1;
        }
        let settled = ctx.new_native_fn(
            "viewTransitionWaitSettled",
            1,
            Rc::new(|ctx, this, _| {
                ctx.with_instance::<DomViewTransition, _>(&this, |transition| {
                    let mut state = transition.state.borrow_mut();
                    state.wait_pending = state.wait_pending.saturating_sub(1);
                })
                .map_err(|error| error.to_value(ctx))?;
                Ok(Value::Undefined)
            }),
        );
        let settled = ctx
            .bind_function_this(settled, this.0)
            .map_err(OpError::thrown)?;
        ctx.then_value(promise, settled.clone(), settled);
        Ok(())
    }
}

#[lumen_bind::class(name = "ViewTransitionTypeSet", hint(js(webidl)))]
pub struct DomViewTransitionTypeSet {
    data: Rc<RefCell<lumen::embed::DomStringSet>>,
}

/// The `ViewTransitionTypeSet` interface object, with `keys` aliased to
/// `values` as Web IDL requires of a setlike.
pub(crate) fn type_set_constructor(ctx: &mut Ctx) -> Value {
    let constructor = ctx.class_constructor::<DomViewTransitionTypeSet>();
    if let Ok(prototype) = ctx.member_get(&constructor, "prototype") {
        if let Ok(values) = ctx.member_get(&prototype, "values") {
            let _ = ctx.member_set(&prototype, "keys", values);
        }
    }
    constructor
}

impl DomViewTransitionTypeSet {
    fn iterator(&self, ctx: &mut Ctx, owner: Value, entries: bool) -> OpResult<Value> {
        let value = ctx.new_instance(custom_elements::DomCustomStateIterator {
            owner,
            data: self.data.clone(),
            cursor: Cell::new(0),
            done: Cell::new(false),
            entries,
        });
        ctx.set_native_identity_owner::<custom_elements::DomCustomStateIterator>(&value)?;
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomViewTransitionTypeSet {
    #[getter]
    fn size(&self) -> usize {
        self.data.borrow().len()
    }
    #[method(coerce)]
    fn has(&self, value: String) -> bool {
        self.data.borrow().contains(&value)
    }
    #[method(coerce)]
    fn add(&self, this: This<Value>, value: String) -> Value {
        self.data.borrow_mut().insert(value);
        this.0
    }
    #[method(coerce)]
    fn delete(&self, value: String) -> bool {
        self.data.borrow_mut().remove(value)
    }
    fn clear(&self) {
        self.data.borrow_mut().clear();
    }
    #[method(hint(js(also_iterator)))]
    fn values(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.iterator(ctx, this.0, false)
    }
    fn keys(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.iterator(ctx, this.0, false)
    }
    fn entries(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.iterator(ctx, this.0, true)
    }
    fn for_each(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        callback: JsFunction,
        #[default(Value::Undefined)] this_arg: Value,
    ) -> OpResult<()> {
        let mut cursor = 0;
        loop {
            let value = self.data.borrow().next(&mut cursor);
            let Some(value) = value else {
                return Ok(());
            };
            callback.call(ctx, this_arg.clone(), &[value.clone(), value, this.0.clone()])?;
        }
    }
}

/// A promise-typed attribute read on a foreign receiver rejects instead of throwing.
fn promise_attribute(ctx: &mut Ctx, this: &Value, read: impl FnOnce(&TransitionState) -> Value) -> Value {
    match ctx.with_instance::<DomViewTransition, _>(this, |transition| read(&transition.state.borrow())) {
        Ok(promise) => promise,
        Err(error) => {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            deferred.reject(ctx, error);
            promise
        }
    }
}

fn new_transition(ctx: &mut Ctx, callback: Option<Value>) -> OpResult<Value> {
    let wrapper = ctx.new_instance(DomViewTransition {
        state: RefCell::new(TransitionState {
            owner: None,
            scope_root: None,
            captures: Vec::new(),
            roles: Vec::new(),
            types: Rc::new(RefCell::new(lumen::embed::DomStringSet::default())),
            types_wrapper: None,
            wait_pending: 0,
            new_frame_id: None,
            queued: false,
            queued_next: None,
            animations: Vec::new(),
            opacity: [1.0; 2],
            phase: Phase::PendingOldCapture,
            callback,
            callback_invoked: false,
            callback_done: false,
            ready: Settlement::default(),
            update_done: Settlement::default(),
            finished: Settlement::default(),
            old: None,
            new: None,
        }),
    });
    ctx.set_native_identity_owner::<DomViewTransition>(&wrapper)?;
    for slot in 0..3 {
        Deferred::new_registered(ctx, |ctx, deferred| {
            let promise = deferred.promise();
            ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
                let mut state = transition.state.borrow_mut();
                let settlement = match slot {
                    0 => &mut state.ready,
                    1 => &mut state.update_done,
                    _ => &mut state.finished,
                };
                *settlement = Settlement {
                    promise,
                    deferred: Some(deferred),
                };
            })
            .expect("new native transition identity");
        });
    }
    Ok(wrapper)
}

/// Run exactly once from the admitted HTML update task, including when a
/// transition was skipped before its old capture could be taken.
fn invoke_update(ctx: &mut Ctx, wrapper: &Value) -> OpResult<()> {
    let callback = ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        if state.callback_invoked {
            return None;
        }
        state.callback_invoked = true;
        if state.phase != Phase::Skipped {
            state.phase = Phase::Updating;
        }
        Some(state.callback.take())
    })?;
    let Some(callback) = callback else {
        return Ok(());
    };
    let result = match callback {
        Some(callback) => ctx.invoke(callback, Value::Undefined, &[]),
        None => Ok(Value::Undefined),
    };
    match result {
        Err(reason) => settle_update(ctx, wrapper, Err(reason)),
        Ok(value) => {
            // Bound receiver is a GC-visible edge from the engine reaction;
            // native closures do not retain an invisible wrapper reference.
            let complete = ctx.new_native_fn(
                "viewTransitionUpdateComplete",
                1,
                Rc::new(|ctx, this, _| {
                    settle_update(ctx, &this, Ok(())).map_err(|error| error.to_value(ctx))?;
                    Ok(Value::Undefined)
                }),
            );
            let failed = ctx.new_native_fn(
                "viewTransitionUpdateFailed",
                1,
                Rc::new(|ctx, this, args| {
                    settle_update(
                        ctx,
                        &this,
                        Err(args.first().cloned().unwrap_or(Value::Undefined)),
                    )
                    .map_err(|error| error.to_value(ctx))?;
                    Ok(Value::Undefined)
                }),
            );
            let complete = ctx
                .bind_function_this(complete, wrapper.clone())
                .map_err(OpError::thrown)?;
            let failed = ctx
                .bind_function_this(failed, wrapper.clone())
                .map_err(OpError::thrown)?;
            ctx.then_value(value, complete, failed);
            Ok(())
        }
    }
}

fn settle_update(ctx: &mut Ctx, wrapper: &Value, outcome: Result<(), Value>) -> OpResult<()> {
    let (update, ready, finished) =
        ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            if state.callback_done {
                return (None, None, None);
            }
            state.callback_done = true;
            let update = state.update_done.deferred.take();
            if outcome.is_err() {
                state.phase = Phase::Done;
                state.old = None;
                state.new = None;
                state.new_frame_id = None;
                (
                    update,
                    state.ready.deferred.take(),
                    state.finished.deferred.take(),
                )
            } else if state.phase == Phase::Skipped {
                state.phase = Phase::Done;
                (update, None, state.finished.deferred.take())
            } else {
                state.phase = Phase::PendingNewCapture;
                (update, None, None)
            }
        })?;
    match outcome {
        Ok(()) => {
            if let Some(update) = update {
                update.resolve(ctx, Value::Undefined);
            }
            if let Some(finished) = finished {
                finished.resolve(ctx, Value::Undefined);
            }
        }
        Err(reason) => {
            if let Some(update) = update {
                update.reject(ctx, OpError::thrown(reason.clone()));
            }
            if let Some(ready) = ready {
                ready.reject_handled(ctx, reason.clone());
            }
            if let Some(finished) = finished {
                finished.reject(ctx, OpError::thrown(reason));
            }
        }
    }
    let done = ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        transition.state.borrow().phase == Phase::Done
    })?;
    if done {
        clear_presentation(ctx, wrapper)?;
    }
    Ok(())
}

// The Document's actual JS wrapper owns its active transition and callback
// queue through traced private edges. Native owners and task closures are weak.
const ACTIVE: &str = "#\u{0}viewTransitionActive";
const QUEUE_HEAD: &str = "#\u{0}viewTransitionUpdateHead";
const QUEUE_TAIL: &str = "#\u{0}viewTransitionUpdateTail";
const QUEUE_COUNT: &str = "#\u{0}viewTransitionUpdateCount";

fn slot(ctx: &mut Ctx, owner: &Value, key: &str) -> OpResult<Value> {
    ctx.member_get(owner, key).map_err(OpError::thrown)
}

struct StartArguments {
    callback: Option<Value>,
    types: Vec<String>,
}

/// `(ViewTransitionUpdateCallback or StartViewTransitionOptions)` conversion.
fn start_arguments(ctx: &mut Ctx, argument: Option<Value>) -> OpResult<StartArguments> {
    let none = StartArguments {
        callback: None,
        types: Vec::new(),
    };
    let Some(argument) = argument else {
        return Ok(none);
    };
    if argument.is_callable() {
        return Ok(StartArguments {
            callback: Some(argument),
            types: Vec::new(),
        });
    }
    match &argument {
        Value::Undefined | Value::Null => Ok(none),
        Value::Obj(_) => {
            let update = ctx.member_get(&argument, "update").map_err(OpError::thrown)?;
            let callback = match update {
                Value::Undefined | Value::Null => None,
                value if value.is_callable() => Some(value),
                _ => {
                    return Err(OpError::type_error(
                        "StartViewTransitionOptions.update must be a function",
                    ))
                }
            };
            let types = ctx.member_get(&argument, "types").map_err(OpError::thrown)?;
            let types = match types {
                Value::Undefined => Vec::new(),
                value => ctx.convert_iterable(&value, 65_536, |ctx, item| {
                    Ok(ctx.coerce_string(&item).map_err(OpError::thrown)?.to_string())
                })?,
            };
            Ok(StartArguments { callback, types })
        }
        _ => Err(OpError::type_error(
            "startViewTransition argument must be a function or an options object",
        )),
    }
}

fn new_started_transition(
    ctx: &mut Ctx,
    document: &Rc<DomRealm>,
    scope_root: Option<NodeId>,
    argument: Option<Value>,
) -> OpResult<Value> {
    let arguments = start_arguments(ctx, argument)?;
    let wrapper = new_transition(ctx, arguments.callback)?;
    ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        state.owner = Some(Rc::downgrade(document));
        state.scope_root = scope_root;
        let mut types = state.types.borrow_mut();
        for kind in arguments.types {
            types.insert(kind);
        }
    })?;
    Ok(wrapper)
}

pub(crate) fn start(
    ctx: &mut Ctx,
    document: &Rc<DomRealm>,
    argument: Option<Value>,
) -> OpResult<Value> {
    let wrapper = new_started_transition(ctx, document, None, argument)?;
    document.view_transition_started.set(true);
    let owner = document.document_value(ctx);
    let active = slot(ctx, &owner, ACTIVE)?;
    if active.as_obj().is_some() {
        skip(
            ctx,
            &active,
            "AbortError",
            "View transition replaced by another transition",
        )?;
    }
    ctx.set_native_internal_value_slot(&owner, ACTIVE, wrapper.clone())
        .map_err(OpError::thrown)?;
    if document.lifecycle.destroyed.get() || document.lifecycle.hidden.get() {
        skip(
            ctx,
            &wrapper,
            "InvalidStateError",
            "View transition document is not visible",
        )?;
    }
    Ok(wrapper)
}

/// Element-scoped transitions have no scoped snapshot provider: the update
/// callback runs and the transition is skipped like any uncapturable one.
pub(crate) fn start_scoped(
    ctx: &mut Ctx,
    document: &Rc<DomRealm>,
    node: NodeId,
    argument: Option<Value>,
) -> OpResult<Value> {
    let wrapper = new_started_transition(ctx, document, Some(node), argument)?;
    skip(
        ctx,
        &wrapper,
        "InvalidStateError",
        "Element-scoped view transitions cannot be captured",
    )?;
    Ok(wrapper)
}

pub(crate) fn active(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<Value> {
    let owner = document.document_value(ctx);
    let active = slot(ctx, &owner, ACTIVE)?;
    Ok(if active.as_obj().is_some() {
        active
    } else {
        Value::Null
    })
}

fn owner(ctx: &mut Ctx, wrapper: &Value) -> OpResult<Option<Rc<DomRealm>>> {
    ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        transition
            .state
            .borrow()
            .owner
            .as_ref()
            .and_then(Weak::upgrade)
    })
}

fn schedule_update(ctx: &mut Ctx, wrapper: &Value) -> OpResult<()> {
    let needed = ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        let state = transition.state.borrow();
        !state.callback_invoked && !state.queued
    })?;
    if !needed {
        return Ok(());
    }
    let Some(document) = owner(ctx, wrapper)? else {
        return Ok(());
    };
    let document_wrapper = document.document_value(ctx);
    let count = match slot(ctx, &document_wrapper, QUEUE_COUNT)? {
        Value::Num(count) => count as usize,
        _ => 0,
    };
    if count >= scheduling::MAX_PENDING_HTML_TASKS {
        return Err(OpError::new(
            "QuotaExceededError",
            "View transition callback queue is full",
        ));
    }
    let weak = Rc::downgrade(&document);
    // Admission precedes publication; canceled tasks never retain native Doc
    // or opaque JS roots. The Document queue supplies the actual JS edges.
    scheduling::queue_task(ctx, move |ctx| {
        if let Some(document) = weak.upgrade() {
            flush_updates(ctx, &document)?;
        }
        Ok(())
    })?;
    let tail = slot(ctx, &document_wrapper, QUEUE_TAIL)?;
    if tail.as_obj().is_some() {
        ctx.with_instance::<DomViewTransition, _>(&tail, |transition| {
            transition.state.borrow_mut().queued_next = Some(wrapper.clone());
        })?;
    } else {
        ctx.set_native_internal_value_slot(&document_wrapper, QUEUE_HEAD, wrapper.clone())
            .map_err(OpError::thrown)?;
    }
    ctx.set_native_internal_value_slot(&document_wrapper, QUEUE_TAIL, wrapper.clone())
        .map_err(OpError::thrown)?;
    ctx.set_native_internal_value_slot(
        &document_wrapper,
        QUEUE_COUNT,
        Value::Num((count + 1) as f64),
    )
    .map_err(OpError::thrown)?;
    ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        transition.state.borrow_mut().queued = true
    })?;
    Ok(())
}

fn flush_updates(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    let document_wrapper = document.document_value(ctx);
    let mut next = slot(ctx, &document_wrapper, QUEUE_HEAD)?;
    for key in [QUEUE_HEAD, QUEUE_TAIL, QUEUE_COUNT] {
        ctx.set_native_internal_value_slot(&document_wrapper, key, Value::Null)
            .map_err(OpError::thrown)?;
    }
    let mut first_error = None;
    // A snapshot prevents callbacks appended during invocation from running
    // before the current callback has returned to its task boundary.
    while next.as_obj().is_some() {
        let wrapper = next;
        next = ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            state.queued = false;
            state.queued_next.take().unwrap_or(Value::Null)
        })?;
        if let Err(error) = invoke_update(ctx, &wrapper) {
            if first_error.is_none() {
                first_error = Some(error);
            }
            let _ = skip(ctx, &wrapper, "AbortError", "View transition update failed");
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn clear_presentation(ctx: &mut Ctx, wrapper: &Value) -> OpResult<()> {
    let (document, animations) =
        ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            state.old = None;
            state.new = None;
            state.new_frame_id = None;
            (
                state.owner.as_ref().and_then(Weak::upgrade),
                std::mem::take(&mut state.animations),
            )
        })?;
    let mut failure = None;
    for (id, _, _, _) in animations {
        if let Err(error) = animations::cancel_capture_animation(ctx, id) {
            if failure.is_none() {
                failure = Some(error);
            }
        }
    }
    if let Some(document) = document {
        let document_wrapper = document.document_value(ctx);
        if slot(ctx, &document_wrapper, ACTIVE)?.object_identity() == wrapper.object_identity() {
            document.view_transition_captures.borrow_mut().clear();
            document
                .browser_services
                .render_capture
                .suppress_hit_testing
                .set(false);
            document.session.borrow_mut().set_capture_overlay(None);
            ctx.set_native_internal_value_slot(&document_wrapper, ACTIVE, Value::Null)
                .map_err(OpError::thrown)?;
        }
    }
    failure.map_or(Ok(()), Err)
}

fn skip(ctx: &mut Ctx, wrapper: &Value, name: &'static str, message: &str) -> OpResult<()> {
    let (ready, finished, changed) =
        ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            if matches!(state.phase, Phase::Done | Phase::Skipped) {
                return (None, None, false);
            }
            state.phase = Phase::Skipped;
            let ready = state.ready.deferred.take();
            let finished = if state.callback_done {
                state.finished.deferred.take()
            } else {
                None
            };
            (ready, finished, true)
        })?;
    if !changed {
        return Ok(());
    }
    let queued = schedule_update(ctx, wrapper);
    let cleared = clear_presentation(ctx, wrapper);
    if let Some(ready) = ready {
        let reason = crate::error_reporting::dom_exception(ctx, name, message).to_value(ctx);
        ready.reject_handled(ctx, reason);
    }
    if let Some(finished) = finished {
        finished.resolve(ctx, Value::Undefined);
    }
    if let Err(error) = queued {
        let reason = error.to_value(ctx);
        settle_update(ctx, wrapper, Err(reason.clone()))?;
        return Err(OpError::thrown(reason));
    }
    cleared
}

pub(crate) fn pending(ctx: &mut Ctx) -> bool {
    let Some(document) = window_globals::current_dom_realm(ctx) else {
        return false;
    };
    let wrapper = document.document_value(ctx);
    let Ok(active) = slot(ctx, &wrapper, ACTIVE) else {
        return false;
    };
    if active.as_obj().is_none() {
        return false;
    }
    ctx.with_instance::<DomViewTransition, _>(&active, |transition| {
        matches!(
            transition.state.borrow().phase,
            Phase::PendingOldCapture | Phase::PendingNewCapture | Phase::Animating
        )
    })
    .unwrap_or(false)
}

fn capture_request(document: &Rc<DomRealm>) -> OpResult<RenderCaptureRequest> {
    let (width, height) = document.session.borrow().viewport_size().ok_or_else(|| {
        OpError::new(
            "InvalidStateError",
            "View transition has no rendered viewport",
        )
    })?;
    Ok(RenderCaptureRequest {
        target: RenderCaptureTarget::Viewport,
        width,
        height,
    })
}

fn overlay(document: &Rc<DomRealm>, image: Arc<ReservedImageData>) {
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        width: image.image.width as f32,
        height: image.image.height as f32,
    };
    document
        .session
        .borrow_mut()
        .set_capture_overlay(Some(DisplayList(vec![Command::ReservedImage {
            rect,
            image,
        }])));
}

/// HTML pending-transition operations run after the ordinary style/layout
/// update. Capture providers deliberately exclude this presentation overlay.
pub(crate) fn rendering_checkpoint(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    if !document.view_transition_started.get() {
        return Ok(());
    }
    let document_wrapper = document.document_value(ctx);
    let wrapper = slot(ctx, &document_wrapper, ACTIVE)?;
    if wrapper.as_obj().is_none() {
        return Ok(());
    }
    if document.lifecycle.hidden.get() {
        return skip(
            ctx,
            &wrapper,
            "InvalidStateError",
            "View transition document became hidden",
        );
    }
    let phase = ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
        transition.state.borrow().phase
    })?;
    let result = match phase {
        Phase::PendingOldCapture => setup_old(ctx, document, &wrapper),
        Phase::PendingNewCapture => setup_new(ctx, document, &wrapper),
        _ => Ok(()),
    };
    if let Err(error) = result {
        let reason = error.to_value(ctx);
        skip_reason(ctx, &wrapper, reason)?;
    }
    Ok(())
}

fn skip_reason(ctx: &mut Ctx, wrapper: &Value, reason: Value) -> OpResult<()> {
    let ready = ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        transition.state.borrow_mut().ready.deferred.take()
    })?;
    let result = skip(
        ctx,
        wrapper,
        "InvalidStateError",
        "View transition capture failed",
    );
    if let Some(ready) = ready {
        ready.reject_handled(ctx, reason);
    }
    result
}

fn viewport_origin(document: &Rc<DomRealm>) -> OpResult<NodeId> {
    document
        .with_session(|session| selector::document_element(session.document()))
        .ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "View transition document has no root element",
            )
        })
}

fn capture_name(
    document: &lumen_html::Document,
    node: NodeId,
    declared: &css::ViewTransitionName,
) -> Option<Arc<str>> {
    match declared {
        css::ViewTransitionName::None => None,
        css::ViewTransitionName::MatchElement => Some(Arc::from(format!(
            "-ua-match-element-{}",
            node.key()
        ))),
        css::ViewTransitionName::Custom(name) if &**name == "auto" => {
            let id = document
                .get_attribute_ns_ref(node, None, "id")
                .ok()
                .flatten()
                .filter(|id| !id.is_empty());
            Some(Arc::from(match id {
                Some(id) => format!("-ua-id-{id}"),
                None => format!("-ua-match-element-{}", node.key()),
            }))
        }
        css::ViewTransitionName::Custom(name) => Some(name.clone()),
    }
}

fn capture_rect(session: &lumen_html::session::RenderSession, root: NodeId, node: NodeId) -> [f32; 4] {
    if node == root {
        return session
            .viewport_size()
            .map_or([0.0; 4], |(width, height)| [0.0, 0.0, width as f32, height as f32]);
    }
    session
        .layout_rect_with_replacement(node)
        .map_or([0.0; 4], |(rect, _)| [rect.x, rect.y, rect.width, rect.height])
}

/// The ordered set of captured elements: the root first, then every other
/// rendered element with a `view-transition-name`, in tree order.
fn collect_captures(document: &Rc<DomRealm>, root: NodeId) -> OpResult<Vec<CapturedElement>> {
    let mut session = document.session.borrow_mut();
    let snapshot = session.transition_snapshot().map_err(|error| {
        OpError::new(
            "InvalidStateError",
            format!("View transition styles: {error:?}"),
        )
    })?;
    let root_style = Arc::new(session.computed_style(root).map_err(|error| {
        OpError::new(
            "InvalidStateError",
            format!("View transition root style: {error:?}"),
        )
    })?);
    let mut captures = Vec::new();
    if let Some(name) = capture_name(session.document(), root, &root_style.view_transition_name) {
        captures.push(CapturedElement {
            name,
            node: root,
            rect: capture_rect(&session, root, root),
            style: root_style.clone(),
        });
    }
    for target in &snapshot.nodes {
        if target.node == root || target.pseudo.is_some() || !target.was_rendered() {
            continue;
        }
        let Some(name) = capture_name(
            session.document(),
            target.node,
            &target.style.view_transition_name,
        ) else {
            continue;
        };
        captures.push(CapturedElement {
            name,
            node: target.node,
            rect: capture_rect(&session, root, target.node),
            style: target.style.clone(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for capture in &captures {
        if !seen.insert(capture.name.clone()) {
            return Err(OpError::new(
                "InvalidStateError",
                "Duplicate view-transition-name",
            ));
        }
    }
    Ok(captures)
}

fn setup_old(ctx: &mut Ctx, document: &Rc<DomRealm>, wrapper: &Value) -> OpResult<()> {
    flush_updates(ctx, document)?;
    let document_wrapper = document.document_value(ctx);
    if slot(ctx, &document_wrapper, ACTIVE)?.object_identity()
        != wrapper.object_identity()
    {
        return Ok(());
    }
    let _ = document.flush_layout();
    let root = viewport_origin(document)?;
    let captures = collect_captures(document, root)?;
    let old = match capture_request(document) {
        Ok(request) => {
            let budget = document.render_capture_pixel_budget()?;
            Some(document.render_capture_reserved(&request, &budget)?)
        }
        Err(_) => None,
    };
    ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        state.old = old.clone();
        state.captures = captures;
        state.phase = Phase::Updating;
    })?;
    if let Some(old) = old {
        document
            .browser_services
            .render_capture
            .suppress_hit_testing
            .set(true);
        overlay(document, old);
    }
    schedule_update(ctx, wrapper)
}

fn keyframe(offset: f64, declarations: &[(&str, String)]) -> Keyframe {
    Keyframe {
        offset_is_specified: true,
        offset,
        declarations: declarations
            .iter()
            .map(|(property, value)| ((*property).into(), value.as_str().into()))
            .collect(),
        easing: None,
        composite: None,
    }
}

fn fade_frames(from: &str, to: &str) -> Vec<Keyframe> {
    vec![
        keyframe(0.0, &[("opacity", from.to_owned())]),
        keyframe(1.0, &[("opacity", to.to_owned())]),
    ]
}

fn blend_frames() -> Vec<Keyframe> {
    let plus_lighter = || [("mix-blend-mode", String::from("plus-lighter"))];
    vec![keyframe(0.0, &plus_lighter()), keyframe(1.0, &plus_lighter())]
}

fn group_frames(old: [f32; 4], new: [f32; 4]) -> Vec<Keyframe> {
    let state = |rect: [f32; 4]| {
        [
            (
                "transform",
                format!("matrix(1, 0, 0, 1, {}, {})", rect[0], rect[1]),
            ),
            ("width", format!("{}px", rect[2])),
            ("height", format!("{}px", rect[3])),
        ]
    };
    vec![keyframe(0.0, &state(old)), keyframe(1.0, &state(new))]
}

/// A user-agent animation applies only while the cascade leaves
/// `animation-name` at its user-agent value.
fn user_agent_animates(style: &css::Style) -> bool {
    style.animation[0].is_none()
}

struct CaptureAnimation<'a> {
    pseudo: PseudoElement,
    style: &'a css::Style,
    index: usize,
    name: String,
    role: Role,
    frames: Vec<Keyframe>,
}

fn start_capture_animation(
    ctx: &mut Ctx,
    document: &Rc<DomRealm>,
    wrapper: &Value,
    root: NodeId,
    capture: &Arc<str>,
    animation: CaptureAnimation<'_>,
) -> OpResult<()> {
    let (id, handle) = animations::create_capture_animation(
        ctx,
        document,
        root,
        animation.pseudo,
        capture.clone(),
        &animation.name,
        &animation.style.animation,
        animation.index,
        animation.frames,
    )?;
    ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        state.animations.push((
            id,
            handle,
            animation.role == Role::FadeOld,
            f64::from(animation.style.opacity),
        ));
        state.roles.push(animation.role);
    })
}

/// The computed styles of the `::view-transition` tree for one capture name,
/// each inheriting from its parent pseudo-element: the tree root, then (when a
/// name is given) group, image pair, old and new.
fn pseudo_chain(
    session: &mut lumen_html::session::RenderSession,
    root: NodeId,
    name: Option<&str>,
    text: &dyn lumen_html::paint::TextShaper,
    active: Option<&ActiveCapture>,
) -> Result<Vec<css::Style>, lumen_html::layout::LayoutError> {
    let root_style = session.computed_style_with_text(root, Some(text))?;
    let tree = session.view_transition_pseudo_style(
        root,
        Some(&root_style),
        PseudoElement::ViewTransition,
        None,
        text,
    )?;
    let Some(name) = name else {
        return Ok(vec![tree]);
    };
    let mut group = session.view_transition_pseudo_style(
        root,
        Some(&tree),
        PseudoElement::ViewTransitionGroup,
        Some(name),
        text,
    )?;
    if let Some(active) = active {
        group.adopt_captured_group_properties(&active.source);
    }
    let mut pair = session.view_transition_pseudo_style(
        root,
        Some(&group),
        PseudoElement::ViewTransitionImagePair,
        Some(name),
        text,
    )?;
    let mut old = session.view_transition_pseudo_style(
        root,
        Some(&pair),
        PseudoElement::ViewTransitionOld,
        Some(name),
        text,
    )?;
    let mut new = session.view_transition_pseudo_style(
        root,
        Some(&pair),
        PseudoElement::ViewTransitionNew,
        Some(name),
        text,
    )?;
    if let Some(active) = active {
        for (style, pseudo) in [
            (&mut group, PseudoElement::ViewTransitionGroup),
            (&mut pair, PseudoElement::ViewTransitionImagePair),
            (&mut old, PseudoElement::ViewTransitionOld),
            (&mut new, PseudoElement::ViewTransitionNew),
        ] {
            style.apply_view_transition_state(pseudo, &active.name, active.old, active.new);
        }
    }
    Ok(vec![tree, group, pair, old, new])
}

/// The computed style `getComputedStyle(root, pseudo)` reports for a
/// view-transition pseudo-element.
pub(crate) fn computed_pseudo_style(
    realm: &Rc<DomRealm>,
    session: &mut lumen_html::session::RenderSession,
    root: NodeId,
    pseudo: PseudoElement,
    name: Option<&str>,
    text: &dyn lumen_html::paint::TextShaper,
) -> Result<css::Style, lumen_html::layout::LayoutError> {
    let active = name.and_then(|name| {
        realm
            .view_transition_captures
            .borrow()
            .iter()
            .find(|capture| &*capture.name == name)
            .cloned()
    });
    let mut chain = pseudo_chain(session, root, name, text, active.as_ref())?;
    let index = match pseudo {
        PseudoElement::ViewTransition => 0,
        PseudoElement::ViewTransitionGroup => 1,
        PseudoElement::ViewTransitionImagePair => 2,
        PseudoElement::ViewTransitionOld => 3,
        _ => 4,
    };
    if index >= chain.len() {
        return Err(lumen_html::layout::LayoutError::InvalidTree);
    }
    Ok(chain.swap_remove(index))
}

fn pseudo_styles(
    document: &Rc<DomRealm>,
    root: NodeId,
    name: &str,
    fonts: &crate::canvas::CanvasFontSource,
) -> OpResult<[css::Style; 3]> {
    let mut session = document.session.borrow_mut();
    let chain = pseudo_chain(&mut session, root, Some(name), fonts, None).map_err(|error| {
        OpError::new(
            "InvalidStateError",
            format!("View transition pseudo style: {error:?}"),
        )
    })?;
    let [_, group, _, old, new]: [css::Style; 5] = chain
        .try_into()
        .map_err(|_| OpError::new("InvalidStateError", "View transition pseudo chain"))?;
    Ok([group, old, new])
}

fn setup_new(ctx: &mut Ctx, document: &Rc<DomRealm>, wrapper: &Value) -> OpResult<()> {
    let _ = document.flush_layout();
    let root = viewport_origin(document)?;
    let new_captures = collect_captures(document, root)?;
    let (old_captures, old_image) =
        ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
            let state = transition.state.borrow();
            (state.captures.clone(), state.old.clone())
        })?;
    let viewport_only = old_image.is_some()
        && old_captures.len() == new_captures.len()
        && old_captures
            .iter()
            .zip(&new_captures)
            .all(|(old, new)| old.node == root && new.node == root && old.name == new.name);
    let mut new_image = None;
    let mut new_frame_id = None;
    if viewport_only && !new_captures.is_empty() {
        let request = capture_request(document)?;
        let dimensions = old_image
            .as_ref()
            .map(|old| (old.image.width, old.image.height));
        if dimensions != Some((request.width, request.height)) {
            return Err(OpError::new(
                "InvalidStateError",
                "View transition viewport dimensions changed",
            ));
        }
        let budget = document.render_capture_pixel_budget()?;
        new_image = Some(document.render_capture_reserved(&request, &budget)?);
        new_frame_id = Some(document.session.borrow().frame_id());
    }
    document
        .browser_services
        .render_capture
        .suppress_hit_testing
        .set(false);
    if new_image.is_none() {
        document.session.borrow_mut().set_capture_overlay(None);
    }
    if old_captures.is_empty() && new_captures.is_empty() {
        let (ready, finished) =
            ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
                let mut state = transition.state.borrow_mut();
                state.phase = Phase::Done;
                (state.ready.deferred.take(), state.finished.deferred.take())
            })?;
        clear_presentation(ctx, wrapper)?;
        if let Some(ready) = ready {
            ready.resolve(ctx, Value::Undefined);
        }
        if let Some(finished) = finished {
            finished.resolve(ctx, Value::Undefined);
        }
        return Ok(());
    }
    let mut names: Vec<(Arc<str>, Option<CapturedElement>, Option<CapturedElement>)> = old_captures
        .iter()
        .map(|old| {
            let new = new_captures.iter().find(|new| new.name == old.name);
            (old.name.clone(), Some(old.clone()), new.cloned())
        })
        .collect();
    for new in &new_captures {
        if !old_captures.iter().any(|old| old.name == new.name) {
            names.push((new.name.clone(), None, Some(new.clone())));
        }
    }
    let fonts = crate::canvas::realm_font_source(document)?;
    let mut active = Vec::with_capacity(names.len());
    for (name, old, new) in &names {
        let source = new.as_ref().or(old.as_ref()).map(|capture| capture.style.clone());
        if let Some(source) = source {
            active.push(ActiveCapture {
                name: name.clone(),
                old: old.is_some(),
                new: new.is_some(),
                source,
            });
        }
        let [group, old_style, new_style] = pseudo_styles(document, root, name, &fonts)?;
        if viewport_only {
            ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
                transition.state.borrow_mut().opacity =
                    [f64::from(old_style.opacity), f64::from(new_style.opacity)];
            })?;
        }
        if let (Some(old), Some(new)) = (old, new) {
            if user_agent_animates(&group) {
                start_capture_animation(
                    ctx,
                    document,
                    wrapper,
                    root,
                    name,
                    CaptureAnimation {
                        pseudo: PseudoElement::ViewTransitionGroup,
                        style: &group,
                        index: 0,
                        name: format!("-ua-view-transition-group-anim-{name}"),
                        role: Role::Group,
                        frames: group_frames(old.rect, new.rect),
                    },
                )?;
            }
        }
        for (present, other, pseudo, style, fade, role, from, to) in [
            (
                old.is_some(),
                new.is_some(),
                PseudoElement::ViewTransitionOld,
                &old_style,
                "-ua-view-transition-fade-out",
                Role::FadeOld,
                "1",
                "0",
            ),
            (
                new.is_some(),
                old.is_some(),
                PseudoElement::ViewTransitionNew,
                &new_style,
                "-ua-view-transition-fade-in",
                Role::FadeNew,
                "0",
                "1",
            ),
        ] {
            if !present || !user_agent_animates(style) {
                continue;
            }
            start_capture_animation(
                ctx,
                document,
                wrapper,
                root,
                name,
                CaptureAnimation {
                    pseudo,
                    style,
                    index: 0,
                    name: fade.to_owned(),
                    role,
                    frames: fade_frames(from, to),
                },
            )?;
            if other {
                start_capture_animation(
                    ctx,
                    document,
                    wrapper,
                    root,
                    name,
                    CaptureAnimation {
                        pseudo,
                        style,
                        index: 1,
                        name: String::from("-ua-mix-blend-mode-plus-lighter"),
                        role: Role::Blend,
                        frames: blend_frames(),
                    },
                )?;
            }
        }
    }
    let ready = ctx.with_instance::<DomViewTransition, _>(wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        if new_image.is_none() {
            state.old = None;
        }
        state.new = new_image;
        state.new_frame_id = new_frame_id;
        state.phase = Phase::Animating;
        state.ready.deferred.take()
    })?;
    *document.view_transition_captures.borrow_mut() = active;
    if let Some(ready) = ready {
        ready.resolve(ctx, Value::Undefined);
    }
    Ok(())
}

/// Samples the same CSSAnimation records as the author-visible effect API,
/// after the canonical timeline advances and before author RAF callbacks.
pub(crate) fn advance(ctx: &mut Ctx) -> OpResult<()> {
    if let Err(error) = advance_inner(ctx) {
        let reason = error.to_value(ctx);
        if let Some(document) = window_globals::current_dom_realm(ctx) {
            let document_wrapper = document.document_value(ctx);
            let wrapper = slot(ctx, &document_wrapper, ACTIVE)?;
            if wrapper.as_obj().is_some() {
                skip_reason(ctx, &wrapper, reason)?;
            }
        }
    }
    Ok(())
}

fn advance_inner(ctx: &mut Ctx) -> OpResult<()> {
    let Some(document) = window_globals::current_dom_realm(ctx) else {
        return Ok(());
    };
    rendering_checkpoint(ctx, &document)?;
    let document_wrapper = document.document_value(ctx);
    let wrapper = slot(ctx, &document_wrapper, ACTIVE)?;
    if wrapper.as_obj().is_none() {
        return Ok(());
    }
    let values = ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
        let state = transition.state.borrow();
        if state.phase != Phase::Animating {
            return None;
        }
        Some((
            state.opacity,
            state.old.clone(),
            state.new.clone(),
            state.new_frame_id,
            state
                .animations
                .iter()
                .zip(&state.roles)
                .map(|((id, _, _, opacity), role)| (*id, *role, *opacity))
                .collect::<Vec<_>>(),
            state.wait_pending,
        ))
    })?;
    let Some((opacity, old, new, captured_frame, animations, waiting)) = values else {
        return Ok(());
    };
    let [mut old_opacity, mut new_opacity] = opacity;
    let mut complete = true;
    for (id, role, fallback) in animations {
        let (finished, samples) = animations::capture_animation_sample(ctx, id)?;
        complete &= finished;
        let value = samples
            .iter()
            .find(|(property, _)| property == "opacity")
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .unwrap_or(fallback);
        match role {
            Role::FadeOld => old_opacity = value,
            Role::FadeNew => new_opacity = value,
            Role::Group | Role::Blend => {}
        }
    }
    if complete && waiting == 0 {
        let finished = ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            state.phase = Phase::Done;
            state.finished.deferred.take()
        })?;
        clear_presentation(ctx, &wrapper)?;
        if let Some(finished) = finished {
            finished.resolve(ctx, Value::Undefined);
        }
        return Ok(());
    }
    let (true, Some(old), Some(new)) = (!complete, old, new) else {
        return Ok(());
    };
    // Source mutations invalidate viewport geometry too. Flush the same
    // canonical dependencies before validating dimensions or reading the
    // normal-frame identity; opacity-only opportunities remain cache hits.
    document.prepare_embedded_document_paint()?;
    let request = capture_request(&document)?;
    if (request.width, request.height) != (old.image.width, old.image.height) {
        return skip(
            ctx,
            &wrapper,
            "InvalidStateError",
            "View transition viewport dimensions changed",
        );
    }
    let budget = document.render_capture_pixel_budget()?;
    // The existing normal-frame identity excludes presentation overlays.
    // Reuse the same reserved image for unchanged source/opacity-only frames.
    let frame_id = document.session.borrow().frame_id();
    let new = if captured_frame == Some(frame_id) {
        new
    } else {
        // Release the old live capture before allocating its replacement.
        drop(new);
        ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            state.new = None;
            state.new_frame_id = None;
        })?;
        document.render_capture_reserved(&request, &budget)?
    };
    let frame_id = document.session.borrow().frame_id();
    document.session.borrow_mut().set_capture_overlay(None);
    let image = composite_captures(&old, &new, old_opacity, new_opacity, &budget)?;
    overlay(&document, image);
    ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
        let mut state = transition.state.borrow_mut();
        state.new = Some(new);
        state.new_frame_id = Some(frame_id);
    })?;
    Ok(())
}

pub(crate) fn visibility_changed(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    let document_wrapper = document.document_value(ctx);
    let active = slot(ctx, &document_wrapper, ACTIVE)?;
    if active.as_obj().is_some() {
        skip(
            ctx,
            &active,
            "InvalidStateError",
            "View transition document became hidden",
        )?;
    }
    Ok(())
}

pub(crate) fn retire(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    let Some(handle) = document.relevant_host_realm(ctx) else {
        return Ok(());
    };
    ctx.with_host_realm(&handle, |ctx| retire_inner(ctx, document))
        .map_err(|error| OpError::new("InvalidStateError", error.to_string()))?
}

fn retire_inner(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    let document_wrapper = document.document_value(ctx);
    let active = slot(ctx, &document_wrapper, ACTIVE)?;
    if active.as_obj().is_some() {
        // Retirement does not run an author callback in the replacement Doc.
        ctx.with_instance::<DomViewTransition, _>(&active, |transition| {
            let mut state = transition.state.borrow_mut();
            state.callback = None;
            state.callback_invoked = true;
        })?;
        skip(
            ctx,
            &active,
            "AbortError",
            "View transition document was destroyed",
        )?;
        let reason = crate::error_reporting::dom_exception(
            ctx,
            "AbortError",
            "View transition document was destroyed",
        )
        .to_value(ctx);
        settle_update(ctx, &active, Err(reason))?;
    }
    let mut next = slot(ctx, &document_wrapper, QUEUE_HEAD)?;
    for key in [QUEUE_HEAD, QUEUE_TAIL, QUEUE_COUNT] {
        ctx.set_native_internal_value_slot(&document_wrapper, key, Value::Null)
            .map_err(OpError::thrown)?;
    }
    while next.as_obj().is_some() {
        let wrapper = next;
        next = ctx.with_instance::<DomViewTransition, _>(&wrapper, |transition| {
            let mut state = transition.state.borrow_mut();
            state.callback = None;
            state.callback_invoked = true;
            state.queued_next.take().unwrap_or(Value::Null)
        })?;
        let reason = crate::error_reporting::dom_exception(
            ctx,
            "AbortError",
            "View transition document was destroyed",
        )
        .to_value(ctx);
        settle_update(ctx, &wrapper, Err(reason))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(engine: &mut lumen::Engine, source: &str) -> Value {
        engine
            .eval_value(source)
            .expect("view-transition syntax")
            .unwrap_or_else(|reason| {
                let message = engine
                    .ctx()
                    .coerce_string(&reason)
                    .map(|text| text.to_string())
                    .unwrap_or_else(|_| "unprintable thrown value".into());
                panic!("view-transition evaluation failed: {message}");
            })
    }

    #[test]
    fn rendering_checkpoint_is_skipped_until_a_transition_starts() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<body><div id=target></div></body>", 128).unwrap();
        assert!(!realm.view_transition_started.get());
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(!realm.view_transition_started.get());
        evaluate(&mut engine, "globalThis.t = document.startViewTransition(() => {}); t.ready.catch(() => {}); t.finished.catch(() => {}); t");
        assert!(realm.view_transition_started.get());
        realm.update_rendered_focus(engine.ctx()).unwrap();
    }

    fn viewport(engine: &mut lumen::Engine) -> (Rc<DomRealm>, Rc<lumen_html_text::FontFace>) {
        let realm = crate::install(engine.ctx(), "<style>body{margin:0}#target{width:8px;height:8px;background:red}::view-transition-old(root),::view-transition-new(root){animation-duration:1s;animation-timing-function:linear}</style><div id=target></div>", 128).unwrap();
        let font = Rc::new(
            lumen_html_text::FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap(),
        );
        let layout_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(8, 8, layout_font.as_ref())
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        let capture_font = font.clone();
        realm.set_render_capture_provider(Rc::new(move |session, request| {
            let list = session
                .capture_display_list(request, capture_font.as_ref(), None)
                .map_err(|error| format!("{error:?}"))?;
            lumen_html_image::render_with_font(
                &list,
                request.width,
                request.height,
                1.0,
                true,
                capture_font.as_ref(),
            )
            .map_err(|error| format!("{error:?}"))
        }));
        animations::advance(engine.ctx(), 0.0).unwrap();
        (realm, font)
    }

    fn checkpoint(engine: &mut lumen::Engine) {
        while engine.run_one_job() {}
    }

    #[test]
    fn specification_view_transition_native_task_quota_releases_capture_and_settles_promises() {
        let mut engine = lumen::Engine::new();
        let (realm, _) = viewport(&mut engine);
        evaluate(&mut engine, "globalThis.quotaCalls=0;globalThis.quotaNames=[];globalThis.quotaTransition=document.startViewTransition(()=>quotaCalls++);for(const promise of [quotaTransition.ready,quotaTransition.updateCallbackDone,quotaTransition.finished])promise.catch(error=>quotaNames.push(error.name));");
        for _ in 0..scheduling::MAX_PENDING_HTML_TASKS {
            scheduling::queue_task(engine.ctx(), |_| Ok(())).unwrap();
        }
        assert!(realm.update_rendered_focus(engine.ctx()).is_err(), "shared queue admission must remain bounded");
        checkpoint(&mut engine);
        assert_eq!(realm.render_capture_pixel_budget().unwrap().reserved(), 0, "failed task admission must release both capture and presentation owners");
        assert!(!realm.browser_services.render_capture.suppress_hit_testing.get());
        assert!(!pending(engine.ctx()));
        assert!(matches!(evaluate(&mut engine, "quotaCalls===0 && quotaNames.length===3 && quotaNames.every(name=>name==='QuotaExceededError')"), Value::Bool(true)));
        assert!(scheduling::run_tasks(&mut engine, scheduling::MAX_PENDING_HTML_TASKS).is_empty());
    }

    #[test]
    fn specification_view_transition_native_live_capture_reuses_only_unchanged_real_frames() {
        let mut engine=lumen::Engine::new();
        let (realm,font)=viewport(&mut engine);
        let captures=Rc::new(std::cell::Cell::new(0usize));
        let count=captures.clone();
        realm.set_render_capture_provider(Rc::new(move |session,request| {
            count.set(count.get()+1);
            let list=session.capture_display_list(request,font.as_ref(),None)
                .map_err(|error|format!("{error:?}"))?;
            lumen_html_image::render_with_font(&list,request.width,request.height,1.0,true,font.as_ref())
                .map_err(|error|format!("{error:?}"))
        }));
        evaluate(&mut engine,"globalThis.liveTransition=document.startViewTransition(()=>target.style.backgroundColor='blue');liveTransition.ready.catch(()=>{});liveTransition.finished.catch(()=>{});");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(scheduling::run_tasks(&mut engine,8).is_empty());
        checkpoint(&mut engine);
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert_eq!(captures.get(),2,"one old and one new real viewport capture");
        let handle=engine.ctx().current_host_realm();
        let transition=evaluate(&mut engine,"liveTransition");
        for timestamp in [0.0,100.0,250.0] {
            assert!(scheduling::run_animation_frame_in_realm_at(&mut engine,&handle,timestamp).is_empty());
            let phase=engine.ctx().with_instance::<DomViewTransition,_>(&transition,|transition|transition.state.borrow().phase)
                .ok().expect("actual transition phase");
            assert_eq!(phase,Phase::Animating,"real opacity opportunity {timestamp} must preserve an active capture; reserved pixels={}",realm.render_capture_pixel_budget().unwrap().reserved());
        }
        assert_eq!(captures.get(),2,"opacity samples reuse the physical live-new carrier");
        let before_source=(realm.session.borrow().document().version(),realm.session.borrow().frame_id());
        evaluate(&mut engine,"target.style.backgroundColor='lime'");
        let changed_version=realm.session.borrow().document().version();
        assert_ne!(changed_version,before_source.0,"source style mutation changes the actual document input");
        assert!(scheduling::run_animation_frame_in_realm_at(&mut engine,&handle,300.0).is_empty());
        let capture_state=engine.ctx().with_instance::<DomViewTransition,_>(&transition,|transition| {
            let state=transition.state.borrow();
            (state.phase,state.new_frame_id,state.animations.iter().map(|(id,_,_,_)|*id).collect::<Vec<_>>())
        }).ok().expect("live transition state");
        let samples=capture_state.2.iter().map(|id|animations::capture_animation_sample(engine.ctx(),*id).unwrap()).collect::<Vec<_>>();
        assert_eq!(captures.get(),3,"real source style change rerasterizes the live-new view: before={before_source:?}, changed={changed_version}, normal_frame={}, state={capture_state:?}, samples={samples:?}",realm.session.borrow().frame_id());
        let pixels=|engine:&mut lumen::Engine| engine.ctx().with_instance::<DomViewTransition,_>(&transition,|transition| {
            let state=transition.state.borrow();
            state.new.as_ref().unwrap().image.pixels.chunks_exact(4).all(|pixel|pixel==[0,255,0,255])
        }).ok().expect("real transition carrier");
        assert!(pixels(&mut engine),"source mutation reaches the actual reserved capture pixels");
        evaluate(&mut engine,"globalThis.liveCanvas=document.createElement('canvas');liveCanvas.width=8;liveCanvas.height=8;liveCanvas.style.display='block';target.replaceChildren(liveCanvas);globalThis.liveContext=liveCanvas.getContext('2d');liveContext.fillStyle='lime';liveContext.fillRect(0,0,8,8)");
        assert!(scheduling::run_animation_frame_in_realm_at(&mut engine,&handle,350.0).is_empty());
        let count=captures.get();
        let version=realm.session.borrow().document().version();
        evaluate(&mut engine,"liveContext.fillStyle='blue';liveContext.fillRect(0,0,8,8)");
        assert_eq!(realm.session.borrow().document().version(),version,"dirty canvas does not fake a DOM mutation");
        assert!(scheduling::run_animation_frame_in_realm_at(&mut engine,&handle,400.0).is_empty());
        assert_eq!(captures.get(),count+1,"native bitmap publication invalidates the normal frame identity");
        let blue=engine.ctx().with_instance::<DomViewTransition,_>(&transition,|transition| {
            transition.state.borrow().new.as_ref().unwrap().image.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,255,255])
        }).ok().expect("updated transition carrier");
        assert!(blue,"actual canvas pixels reach the live capture");
        assert!(scheduling::run_animation_frame_in_realm_at(&mut engine,&handle,450.0).is_empty());
        assert_eq!(captures.get(),count+1,"unchanged canvas and changing opacity reuse the new carrier");
        evaluate(&mut engine,"liveTransition.skipTransition()");checkpoint(&mut engine);
        assert_eq!(realm.render_capture_pixel_budget().unwrap().reserved(),0);
    }

    #[test]
    fn specification_view_transition_native_capture_tasks_pixels_and_real_animation_completion() {
        let mut engine = lumen::Engine::new();
        let (realm, font) = viewport(&mut engine);
        evaluate(&mut engine, "globalThis.phases=[];globalThis.calls=0;globalThis.transition=document.startViewTransition(()=>{calls++;target.style.backgroundColor='blue'});transition.updateCallbackDone.then(()=>phases.push('update'));transition.ready.then(()=>phases.push('ready'));transition.finished.then(()=>phases.push('finished'));if(calls!==0)throw 'synchronous callback'");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        let target = realm.with_session(|session| {
            selector::get_element_by_id(session.document(), session.document().root(), "target")
                .unwrap()
                .unwrap()
        });
        let root = viewport_origin(&realm).unwrap();
        assert_eq!(
            geometry::element_from_point_in_tree(&realm, None, 4.0, 4.0).unwrap(),
            Some(root),
            "suppressed rendering must hit the document root"
        );
        let before = {
            let list = realm
                .session
                .borrow_mut()
                .display_list(8, 8, font.as_ref())
                .unwrap()
                .clone();
            lumen_html_image::render_with_font(&list, 8, 8, 1.0, true, font.as_ref()).unwrap()
        };
        assert!(before
            .pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 0, 0, 255]));
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        checkpoint(&mut engine);
        realm.update_rendered_focus(engine.ctx()).unwrap();
        let handle = engine.ctx().current_host_realm();
        assert!(scheduling::run_animation_frame_in_realm_at(&mut engine, &handle, 0.0).is_empty());
        assert!(
            scheduling::run_animation_frame_in_realm_at(&mut engine, &handle, 500.0).is_empty()
        );
        let middle = {
            let list = realm
                .session
                .borrow_mut()
                .display_list(8, 8, font.as_ref())
                .unwrap()
                .clone();
            lumen_html_image::render_with_font(&list, 8, 8, 1.0, true, font.as_ref()).unwrap()
        };
        assert!(
            middle
                .pixels
                .chunks_exact(4)
                .all(|pixel| (127..=129).contains(&pixel[0])
                    && pixel[1] == 0
                    && (127..=129).contains(&pixel[2])
                    && pixel[3] == 255),
            "actual capture cross-fade pixels must use canonical animation progress"
        );
        assert_eq!(
            geometry::element_from_point_in_tree(&realm, None, 4.0, 4.0).unwrap(),
            Some(target)
        );
        assert!(
            scheduling::run_animation_frame_in_realm_at(&mut engine, &handle, 1000.0).is_empty()
        );
        checkpoint(&mut engine);
        assert!(matches!(evaluate(&mut engine, "calls===1 && phases.join(',')==='update,ready,finished' && document.getAnimations().every(animation=>animation.playState==='idle')"), Value::Bool(true)));
        assert!(!pending(engine.ctx()));
        assert_eq!(
            realm.render_capture_pixel_budget().unwrap().reserved(),
            0,
            "completed presentation must release capture leases"
        );
    }

    #[test]
    fn specification_view_transition_native_skip_queue_order_failure_and_retired_document() {
        let mut engine = lumen::Engine::new();
        let (realm, _) = viewport(&mut engine);
        evaluate(&mut engine, "globalThis.calls=[];globalThis.errors=[];globalThis.first=document.startViewTransition(()=>calls.push('first'));first.ready.catch(e=>errors.push(e.name));first.finished.then(()=>calls.push('first-finished'));globalThis.second=document.startViewTransition(()=>calls.push('second'));second.skipTransition();second.ready.catch(e=>errors.push(e.name));second.finished.then(()=>calls.push('second-finished'));");
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        checkpoint(&mut engine);
        assert!(matches!(evaluate(&mut engine, "calls.slice(0,2).join(',')==='first,second' && calls.includes('first-finished') && calls.includes('second-finished') && errors.join(',')==='AbortError,AbortError'"), Value::Bool(true)));
        assert_eq!(realm.render_capture_pixel_budget().unwrap().reserved(), 0);
        evaluate(&mut engine, "globalThis.retiredCalls=0;globalThis.retiredErrors=[];globalThis.retired=document.startViewTransition(()=>{retiredCalls++});for(const promise of [retired.ready,retired.updateCallbackDone,retired.finished])promise.catch(error=>retiredErrors.push(error.name));");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        navigation_lifecycle::destroy(engine.ctx(), &realm);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        checkpoint(&mut engine);
        assert!(matches!(evaluate(&mut engine, "retiredCalls===0 && retiredErrors.length===3 && retiredErrors.every(name=>name==='AbortError')"), Value::Bool(true)));
        assert!(!pending(engine.ctx()));
    }

    #[test]
    fn view_transition_compositor_admits_real_scratch_and_retains_actual_alpha_pixels() {
        let budget = lumen_common::limits::ByteBudget::new(20);
        let request = RenderCaptureRequest {
            target: RenderCaptureTarget::Viewport,
            width: 1,
            height: 1,
        };
        let capture = |pixels| {
            ReservedImageData::new(
                lumen_html_image::Rgba8Image {
                    width: 1,
                    height: 1,
                    pixels,
                },
                request.reserve(&budget).unwrap(),
            )
            .unwrap()
        };
        let old = capture(vec![255, 0, 0, 128]);
        let new = capture(vec![0, 0, 255, 128]);
        let guard = budget.reserve(1).unwrap();
        assert!(composite_captures(&old, &new, 0.5, 0.5, &budget).is_err());
        assert_eq!(
            budget.reserved(),
            9,
            "failed scratch admission must release output storage"
        );
        drop(guard);
        let output = composite_captures(&old, &new, 0.5, 0.5, &budget).unwrap();
        assert_eq!(
            budget.reserved(),
            12,
            "surface scratch must release after native composition"
        );
        let pixels = &output.image.pixels;
        assert!(
            (127..=129).contains(&pixels[0])
                && pixels[1] == 0
                && (127..=129).contains(&pixels[2])
                && (127..=129).contains(&pixels[3])
        );
        let retained =
            lumen_html::paint::DisplayList(vec![lumen_html::paint::Command::ReservedImage {
                rect: lumen_html::paint::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                image: output.clone(),
            }]);
        drop(output);
        drop(old);
        drop(new);
        assert_eq!(
            budget.reserved(),
            4,
            "presentation must own its output pixels"
        );
        drop(retained);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn view_transition_constructor_getter_throw_preserves_reason_and_skips_then_lookup() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "", 64).unwrap();
        let callback = engine
            .eval_value(
                r#"globalThis.getterCalls=0; globalThis.reason={marker:1};
            () => { const promise=Promise.resolve(1);
                Object.defineProperty(promise,'constructor',{get(){getterCalls++;throw reason}});
                Object.defineProperty(promise,'then',{get(){throw 'wrong then lookup'}});
                return promise;
            }"#,
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("constructor callback setup threw"));
        let transition = new_transition(engine.ctx(), Some(callback)).unwrap();
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .member_set(&global, "transition", transition.clone())
            .unwrap_or_else(|_| panic!("transition publication failed"));
        engine.eval_value("globalThis.results=[]; transition.ready.catch(e=>results.push(e===reason)); transition.updateCallbackDone.catch(e=>results.push(e===reason)); transition.finished.catch(e=>results.push(e===reason));")
            .unwrap().unwrap_or_else(|_|panic!("constructor observation setup threw"));
        invoke_update(engine.ctx(), &transition).unwrap();
        engine.ctx().collect_garbage();
        while engine.run_one_job() {}
        let result = engine
            .eval_value(
                "getterCalls===1 && results.length===3 && results.every(value=>value===true)",
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("constructor result read threw"));
        assert!(
            matches!(result, Value::Bool(true)),
            "intrinsic await adoption lost constructor getter reason or read then"
        );
    }

    #[test]
    fn view_transition_skip_preserves_one_intrinsic_async_update_and_original_error() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "", 64).unwrap();
        let callback = engine.eval_value("globalThis.calls=0; globalThis.reason={marker:1}; () => { calls++; return Promise.reject(reason); }")
            .unwrap().unwrap_or_else(|_|panic!("transition callback setup threw"));
        let transition = new_transition(engine.ctx(), Some(callback)).unwrap();
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .member_set(&global, "transition", transition.clone())
            .unwrap_or_else(|_| panic!("transition publication failed"));
        engine.eval_value("globalThis.results=[]; transition.ready.catch(e=>results.push(e.name)); transition.updateCallbackDone.catch(e=>results.push(e===reason?'update':'wrong')); transition.finished.catch(e=>results.push(e===reason?'finished':'wrong')); transition.skipTransition(); Promise.prototype.then = () => { throw 'author then must not run'; }")
            .unwrap().unwrap_or_else(|_|panic!("transition observation setup threw"));
        engine.ctx().collect_garbage();
        invoke_update(engine.ctx(), &transition).unwrap();
        invoke_update(engine.ctx(), &transition).unwrap();
        while engine.run_one_job() {}
        let result = engine
            .eval_value("calls===1 && results.join(',')==='AbortError,update,finished'")
            .unwrap()
            .unwrap_or_else(|_| panic!("transition result read threw"));
        let diagnostic = engine
            .eval_value("JSON.stringify({calls,results})")
            .unwrap()
            .unwrap_or_else(|_| panic!("transition diagnostic read threw"));
        let Value::Str(diagnostic) = diagnostic else {
            panic!("transition diagnostic was not a string");
        };
        let phase = engine
            .ctx()
            .with_instance::<DomViewTransition, _>(&transition, |transition| {
                transition.state.borrow().phase
            })
            .unwrap();
        assert!(
            matches!(result, Value::Bool(true)),
            "skip lost callback, reason identity, or native promise ordering: {}; {phase:?}",
            diagnostic.as_str()
        );
    }
}
