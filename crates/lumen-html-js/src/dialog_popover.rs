//! Native dialog/popover methods layered over Lumen's shared top-layer state.
//!
//! This adapter owns Web IDL reflection and script-visible events. Backdrop
//! rendering, modal inertness, and close-watcher integration remain separate
//! renderer/host capabilities. Trusted pointer input calls the shared light-dismiss
//! helper before dispatching pointer and mouse events.

use crate::events::HtmlTargetExt;
use super::{
    browsing_context, error_reporting, events, scheduling, ui_events, DomElement, DomRealm,
};
use lumen::embed::{Ctx, JsHost, OpError, OpResult, Value};
use lumen_bind::{CtorRet, Host};
use lumen_common::toggle_task::{PrepareError, Prepared, ToggleTasks};
use lumen_html::{selector, top_layer, NodeId};
use std::{cell::RefCell, rc::Rc, vec::Vec};

#[lumen_bind::class(
    name = "ToggleEvent",
    extends = super::events::DomEvent,
    hint(js(webidl))
)]
pub(crate) struct DomToggleEvent {
    base: super::events::DomEvent,
    old_state: String,
    new_state: String,
    source_slot: Option<String>,
}

struct ToggleEventConstructor {
    event: DomToggleEvent,
    source: Option<Value>,
}

impl ToggleEventConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let Self { event, source } = self;
        let slot = event.source_slot.clone();
        let instance = ctx.new_instance(event);
        if let (Some(slot), Some(source)) = (slot, source) {
            ctx.define_native_private_value_slot(&instance, &slot, source)?;
        }
        Ok(instance)
    }
}

impl CtorRet<JsHost, DomToggleEvent> for ToggleEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let Self { event, source } = self;
        let slot = event.source_slot.clone();
        let instance = <JsHost as Host>::construct(cx, event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            if let (Some(slot), Some(source)) = (slot, source) {
                ctx.define_native_private_value_slot(&instance, &slot, source)?;
            }
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::methods]
impl DomToggleEvent {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        event_type: &str,
        init: Option<Value>,
    ) -> OpResult<ToggleEventConstructor> {
        // Convert the inherited EventInit dictionary before ToggleEventInit,
        // then read this dictionary's members in Web IDL order.
        let base = events::DomEvent::new(ctx, event_type, init.clone())?;
        let new_state = ui_events::dictionary_string(ctx, &init, "newState", "", false)?;
        let old_state = ui_events::dictionary_string(ctx, &init, "oldState", "", false)?;
        let source = ui_events::dictionary_member(ctx, &init, "source")?.unwrap_or(Value::Null);
        let source = nullable_element(ctx, source)?;
        Ok(toggle_event_constructor(
            ctx, base, old_state, new_state, source,
        ))
    }

    #[getter(name = "oldState")]
    fn old_state(&self) -> String {
        self.old_state.clone()
    }

    #[getter(name = "newState")]
    fn new_state(&self) -> String {
        self.new_state.clone()
    }

    #[getter]
    fn source(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let Some(source) = self
            .source_slot
            .as_deref()
            .and_then(|slot| ctx.native_private_value_slot(&this.0, slot))
        else {
            return Value::Null;
        };
        let Some((source_realm, source_node)) = ctx
            .with_instance::<DomElement, _>(&source, |element| {
                let node = &element.base;
                node.realm.resolve_adopted_node(node.id)
            })
            .ok()
        else {
            return source;
        };
        let active_target = self.base.active_current_target();
        let target_node = active_target.as_ref().and_then(|target| {
            ctx.with_instance::<DomElement, _>(target, |element| {
                let node = &element.base;
                node.realm.resolve_adopted_node(node.id)
            })
            .ok()
        });
        let context = target_node.and_then(|(target_realm, target_node)| {
            Rc::ptr_eq(&source_realm, &target_realm).then_some(target_node)
        });
        let retargeted = source_realm
            .session
            .borrow()
            .document()
            .retarget(source_node, context)
            .ok();
        retargeted.map_or(source, |node| source_realm.wrap(ctx, node))
    }
}

pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
    vec![("ToggleEvent", ctx.class_constructor::<DomToggleEvent>())]
}

fn nullable_element(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if matches!(value, Value::Null | Value::Undefined) {
        return Ok(Value::Null);
    }
    if ctx.with_instance::<DomElement, _>(&value, |_| ()).is_ok() {
        Ok(value)
    } else {
        Err(OpError::type_error(
            "ToggleEventInit.source must be an Element or null",
        ))
    }
}

fn toggle_event_constructor(
    ctx: &mut Ctx,
    base: super::events::DomEvent,
    old_state: String,
    new_state: String,
    source: Value,
) -> ToggleEventConstructor {
    let source = (!matches!(&source, Value::Null | Value::Undefined)).then_some(source);
    let source_slot = source
        .as_ref()
        .map(|_| ctx.allocate_native_private_slot_name());
    ToggleEventConstructor {
        event: DomToggleEvent {
            base,
            old_state,
            new_state,
            source_slot,
        },
        source,
    }
}

pub(crate) fn toggle_event(
    ctx: &mut Ctx,
    event_type: &str,
    old_state: &str,
    new_state: &str,
    cancelable: bool,
    source: Value,
) -> OpResult<Value> {
    let source = nullable_element(ctx, source)?;
    let init = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("bubbles", Value::Bool(false)),
        ("cancelable", Value::Bool(cancelable)),
        ("composed", Value::Bool(false)),
        ("oldState", Value::str(old_state)),
        ("newState", Value::str(new_state)),
    ] {
        ctx.member_set(&init, name, value)
            .map_err(OpError::thrown)?;
    }
    let base = super::events::DomEvent::new(ctx, event_type, Some(init))?;
    toggle_event_constructor(ctx, base, old_state.into(), new_state.into(), source)
        .into_instance(ctx)
        .map_err(OpError::thrown)
}

fn dispatch_toggle(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    event_type: &str,
    old_state: &str,
    new_state: &str,
    cancelable: bool,
    source: Value,
) -> OpResult<bool> {
    let event = toggle_event(ctx, event_type, old_state, new_state, cancelable, source)?;
    let target = realm.wrap(ctx, node);
    realm.dispatch_event_to_target(ctx, node, target, event, true)
}

fn state_error(ctx: &mut Ctx, error: top_layer::StateError) -> OpError {
    error_reporting::dom_exception(
        ctx,
        error.exception_name(),
        "The requested dialog or popover state transition is not available",
    )
}

pub(crate) fn dialog_return_value(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<String> {
    let session = realm.session.borrow();
    session
        .document()
        .dialog_return_value(node)
        .map(str::to_owned)
        .map_err(super::dom_error)
}

pub(crate) fn set_dialog_return_value(
    realm: &Rc<DomRealm>,
    node: NodeId,
    value: &str,
) -> OpResult<()> {
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_dialog_return_value(node, value)
        .map_err(super::dom_error)
}

pub(crate) fn dialog_open(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<bool> {
    let session = realm.session.borrow();
    session
        .document()
        .get_attribute_ns_ref(node, None, "open")
        .map(|value| value.is_some())
        .map_err(super::dom_error)
}

pub(crate) fn set_dialog_open(realm: &Rc<DomRealm>, node: NodeId, open: bool) -> OpResult<()> {
    let mut session = realm.session.borrow_mut();
    if open {
        session
            .document_mut()
            .set_attribute_ns(node, None, "open", "")
            .map_err(super::dom_error)
    } else {
        session
            .document_mut()
            .remove_attribute_ns(node, None, "open")
            .map_err(super::dom_error)
    }
}

pub(crate) fn show_dialog(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    mode: top_layer::DialogMode,
) -> OpResult<()> {
    let (mut realm, mut node) = realm.resolve_adopted_node(node);
    if mode == top_layer::DialogMode::Modal && !has_active_browsing_context(&realm) {
        return Err(state_error(ctx, top_layer::StateError::InvalidState));
    }
    let needs_change = {
        let session = realm.session.borrow();
        top_layer::validate_show_dialog(session.document(), node, mode)
            .map_err(|error| state_error(ctx, error))?
    };
    if !needs_change {
        return Ok(());
    }
    if !dispatch_toggle(
        ctx,
        &realm,
        node,
        "beforetoggle",
        "closed",
        "open",
        true,
        Value::Null,
    )? {
        return Ok(());
    }
    (realm, node) = realm.resolve_adopted_node(node);
    if mode == top_layer::DialogMode::Modal && !has_active_browsing_context(&realm) {
        return Err(state_error(ctx, top_layer::StateError::InvalidState));
    }
    let still_valid = {
        let session = realm.session.borrow();
        top_layer::validate_show_dialog(session.document(), node, mode)
    };
    let still_valid = match still_valid {
        Ok(value) => value,
        Err(top_layer::StateError::InvalidState) if dialog_open(&realm, node)? => return Ok(()),
        Err(error) => return Err(state_error(ctx, error)),
    };
    if !still_valid {
        return Ok(());
    }
    if mode == top_layer::DialogMode::Modal {
        let peers = {
            let session = realm.session.borrow();
            top_layer::all_popovers_to_close(session.document(), node)
        }
        .map_err(|error| state_error(ctx, error))?;
        for &peer in &peers {
            let (peer_realm, peer_node) = realm.resolve_adopted_node(peer);
            dispatch_popover_close_beforetoggle(ctx, &peer_realm, peer_node)?;
            let (peer_realm, peer_node) = peer_realm.resolve_adopted_node(peer_node);
            let closed = {
                let mut session = peer_realm.session.borrow_mut();
                if top_layer::popover_visibility(session.document(), peer_node)
                    != top_layer::PopoverVisibility::Showing
                {
                    false
                } else {
                    top_layer::hide_popover_with_state(session.document_mut(), peer_node)
                        .map_err(|error| state_error(ctx, error))?
                        .0
                }
            };
            if closed {
                queue_toggle_task(
                    ctx,
                    peer_realm,
                    peer_node,
                    ToggleTaskKind::Popover,
                    "open",
                    "closed",
                    Value::Null,
                )?;
            }
            (realm, node) = realm.resolve_adopted_node(node);
            if !has_active_browsing_context(&realm) {
                return Err(state_error(ctx, top_layer::StateError::InvalidState));
            }
        }
        let still_valid = {
            let session = realm.session.borrow();
            top_layer::validate_show_dialog(session.document(), node, mode)
        };
        match still_valid {
            Ok(value) if value => {}
            Ok(_) => return Ok(()),
            Err(top_layer::StateError::InvalidState) if dialog_open(&realm, node)? => return Ok(()),
            Err(error) => return Err(state_error(ctx, error)),
        }
    }
    let restore_focus = (mode == top_layer::DialogMode::Modal)
        .then(|| realm.focused_node())
        .flatten();
    let changed = {
        let mut session = realm.session.borrow_mut();
        top_layer::show_dialog_with_focus(session.document_mut(), node, mode, restore_focus)
    }
    .map_err(|error| state_error(ctx, error))?;
    if changed {
        queue_toggle_task(
            ctx,
            realm.clone(),
            node,
            ToggleTaskKind::Dialog,
            "closed",
            "open",
            Value::Null,
        )?;
        let target = dialog_focus_target(&realm, node)?;
        realm.focus(ctx, Some(target))?;
    }
    Ok(())
}

fn has_active_browsing_context(realm: &DomRealm) -> bool {
    realm
        .browsing_context()
        .is_some_and(|context| browsing_context::is_active_document(&context, realm))
}

pub(crate) fn close_dialog(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    result: Option<&str>,
) -> OpResult<()> {
    let (mut realm, mut node) = realm.resolve_adopted_node(node);
    if !dialog_open(&realm, node)? {
        return Ok(());
    }
    dispatch_toggle(
        ctx,
        &realm,
        node,
        "beforetoggle",
        "open",
        "closed",
        false,
        Value::Null,
    )?;
    (realm, node) = realm.resolve_adopted_node(node);
    if !dialog_open(&realm, node)? {
        return Ok(());
    }
    let transition =
        top_layer::close_dialog_with_state(realm.session.borrow_mut().document_mut(), node)
            .map_err(|error| state_error(ctx, error))?;
    if !transition.changed {
        return Ok(());
    }
    if let Some(result) = result {
        set_dialog_return_value(&realm, node, result)?;
    }
    queue_toggle_task(
        ctx,
        realm.clone(),
        node,
        ToggleTaskKind::Dialog,
        "open",
        "closed",
        Value::Null,
    )?;
    queue_close_event(ctx, realm.clone(), node)?;
    if transition.was_modal {
        if let Some(target) = transition
            .restore_focus
            .filter(|target| realm.session.borrow().document().kind(*target).is_ok())
        {
            realm.focus(ctx, Some(target))?;
        }
    }
    Ok(())
}

fn dialog_focus_target(realm: &Rc<DomRealm>, dialog: NodeId) -> OpResult<NodeId> {
    let mut current = {
        let session = realm.session.borrow();
        session
            .document()
            .first_child(dialog)
            .map_err(super::dom_error)?
    };
    let mut first_focusable = None;
    while let Some(node) = current {
        let (element, autofocus) = {
            let session = realm.session.borrow();
            let element = matches!(
                session.document().kind(node),
                Ok(lumen_html::NodeKind::Element { .. })
            );
            let autofocus = element
                && session
                    .document()
                    .get_attribute_ns_ref(node, None, "autofocus")
                    .map_err(super::dom_error)?
                    .is_some();
            (element, autofocus)
        };
        if element && realm.focusable_node(node)? {
            first_focusable.get_or_insert(node);
            if autofocus {
                return Ok(node);
            }
        }
        current = {
            let session = realm.session.borrow();
            selector::next_shadow_including_descendant(session.document(), dialog, node)
                .map_err(super::dom_error)?
        };
    }
    Ok(first_focusable.unwrap_or(dialog))
}

type ToggleTaskState = ToggleTasks<(NodeId, ToggleTaskKind), &'static str, Value>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToggleTaskKind {
    Dialog,
    Popover,
    Details,
}

/// One document's native notifications, captured before parsing. The document
/// owns this controller; bound task leases preserve it across adoption without
/// retaining the original document or creating a strong cycle.
pub(crate) struct DetailsController {
    sender: scheduling::TaskSender,
    ready: Rc<std::cell::Cell<bool>>,
    realm: RefCell<std::rc::Weak<DomRealm>>,
    pending: RefCell<ToggleTasks<(NodeId, ToggleTaskKind), &'static str, ()>>,
    slots: RefCell<Vec<std::rc::Weak<DetailsSlot>>>,
}

struct DetailsLease {
    realm: Rc<DomRealm>,
    _node: super::NodeRetention,
    _target: Rc<events::TargetData>,
    _wrapper: Option<Value>,
    _document: Option<Value>,
    _controller: Rc<DetailsController>,
}

struct DetailsSlot {
    id: u64,
    node: std::cell::Cell<NodeId>,
    pending: std::cell::Cell<bool>,
    task: RefCell<Option<scheduling::TaskHandle>>,
    controller: std::rc::Weak<DetailsController>,
    lease: RefCell<Option<DetailsLease>>,
}

impl Drop for DetailsSlot {
    fn drop(&mut self) {
        if let Some(controller) = self.controller.upgrade() {
            controller.pending.borrow_mut().cancel(self.id);
        }
    }
}

impl Drop for DetailsController {
    fn drop(&mut self) {
        // Failed preparation owns no leases. Let its weak, now-inert callbacks
        // drain instead of leaving permanently parked queue entries.
        crate::scheduling::mark_task_ready(&self.ready);
    }
}

impl DetailsController {
    pub(crate) fn prepare(ctx: &mut Ctx) -> OpResult<Rc<Self>> {
        Ok(Rc::new(Self {
            sender: scheduling::task_sender(ctx)?,
            ready: Rc::new(std::cell::Cell::new(false)),
            realm: RefCell::new(std::rc::Weak::new()),
            pending: RefCell::new(ToggleTasks::default()),
            slots: RefCell::new(Vec::new()),
        }))
    }

    pub(crate) fn attach(self: &Rc<Self>, document: &mut lumen_html::Document) {
        let controller = self.clone();
        document.set_details_transition_sink(Some(Rc::new(move |_, transition| {
            controller.notify(transition);
        })));
    }

    /// Binding happens only after successful installation, outside a Document
    /// borrow. Parser failures therefore leave no retained native node/global.
    pub(crate) fn bind(self: &Rc<Self>, ctx: &mut Ctx, realm: &Rc<DomRealm>) {
        *self.realm.borrow_mut() = Rc::downgrade(realm);
        *realm.details_controller.borrow_mut() = Rc::downgrade(self);
        for slot in self.slots.borrow().iter().filter_map(std::rc::Weak::upgrade) {
            Self::lease(&slot, realm, self);
            if let Some(lease) = slot.lease.borrow_mut().as_mut() {
                lease._wrapper = Some(realm.wrap(ctx, slot.node.get()));
                lease._document = Some(realm.document_value(ctx));
            }
        }
        crate::scheduling::mark_task_ready(&self.ready);
    }

    fn lease(slot: &DetailsSlot, realm: &Rc<DomRealm>, controller: &Rc<Self>) {
        let target = events::DomEventTarget::node(realm, slot.node.get()).data_handle();
        *slot.lease.borrow_mut() = Some(DetailsLease {
            realm: realm.clone(),
            _node: super::NodeRetention::new(realm, slot.node.get()),
            _target: target,
            _wrapper: realm.wrappers.borrow().get(&slot.node.get()).and_then(lumen::embed::WeakValue::upgrade),
            _document: realm.document_wrapper.borrow().as_ref().and_then(lumen::embed::WeakValue::upgrade),
            _controller: controller.clone(),
        });
    }

    fn failure(&self, cause: scheduling::TaskDiagnosticCause) {
        self.sender.record_failure(scheduling::TaskDiagnosticSource::DetailsToggle, cause);
    }

    fn notify(self: &Rc<Self>, transition: lumen_html::details::DetailsTransition) {
        use scheduling::TaskDiagnosticCause as Cause;
        if !self.sender.is_live() { return; }
        let state = |open| if open { "open" } else { "closed" };
        if let Some(realm) = self.realm.borrow().upgrade() {
            // Adoption preserves the original admitted task and coalescer. A
            // target document holds only a weak routing link to that task slot.
            for slot in self.slots.borrow().iter().filter_map(std::rc::Weak::upgrade) {
                if slot.node.get() == transition.node && slot.pending.get() &&
                    slot.lease.borrow().as_ref().is_some_and(|lease| Rc::ptr_eq(&lease.realm, &realm)) {
                    if let Some(origin) = slot.controller.upgrade() {
                        if origin.pending.borrow_mut().update(slot.id, state(transition.new_open), ()) { return; }
                    }
                }
            }
        }
        let prepared = self.pending.borrow_mut().prepare(
            (transition.node, ToggleTaskKind::Details),
            state(transition.old_open), state(transition.new_open), (),
        );
        let id = match prepared {
            Ok(Prepared::Coalesced) => return,
            Ok(Prepared::New(id)) => id,
            Err(PrepareError::AllocationFailed) => { self.failure(Cause::TrackerAllocationFailed); return; }
            Err(PrepareError::SequenceExhausted) => { self.failure(Cause::TrackerSequenceExhausted); return; }
        };
        {
            let mut slots = self.slots.borrow_mut();
            slots.retain(|slot| slot.strong_count() != 0);
            if slots.try_reserve(1).is_err() {
                self.pending.borrow_mut().cancel(id);
                self.failure(Cause::ProducerAllocationFailed);
                return;
            }
        }
        let slot = Rc::new(DetailsSlot {
            id, node: std::cell::Cell::new(transition.node), controller: Rc::downgrade(self),
            pending: std::cell::Cell::new(true), task: RefCell::new(None), lease: RefCell::new(None),
        });
        let callback_slot = slot.clone();
        let admitted = self.sender.queue_tracked_when_ready(self.ready.clone(), move |ctx| {
            let Some(controller) = callback_slot.controller.upgrade() else { return Ok(()); };
            callback_slot.pending.set(false);
            let Some(task) = controller.pending.borrow_mut().take(callback_slot.id) else { return Ok(()); };
            let lease = callback_slot.lease.borrow_mut().take().ok_or_else(||
                OpError::new("InvalidStateError", "Details toggle target was not bound"))?;
            let (realm, node) = lease.realm.resolve_adopted_node(callback_slot.node.get());
            dispatch_toggle(ctx, &realm, node, "toggle", task.old_state, task.new_state, false, Value::Null)?;
            Ok(())
        });
        match admitted {
            Ok(task) => *slot.task.borrow_mut() = Some(task),
            Err(failure) => {
            self.pending.borrow_mut().cancel(id);
            self.failure(failure.cause);
            // Both references still have empty lease slots, so their release
            // cannot reborrow the Document which is notifying us.
            drop(failure.callback);
            return;
            }
        }
        self.slots.borrow_mut().push(Rc::downgrade(&slot));
        if let Some(realm) = self.realm.borrow().upgrade() {
            Self::lease(&slot, &realm, self);
        }
    }
}

/// Move only native pending ownership; preserve task position and old state.
pub(crate) fn adopt_details_tasks(
    ctx: &mut Ctx, source: &Rc<DomRealm>, target: &Rc<DomRealm>, mapping: &[(NodeId, NodeId)],
) -> OpResult<()> {
    if Rc::ptr_eq(source, target) { return Ok(()); }
    let Some(origin) = source.details_controller.borrow().upgrade() else { return Ok(()); };
    let destination = target.details_controller.borrow().upgrade();
    let destination = match destination {
        Some(controller) => controller,
        None => {
            let controller = DetailsController::prepare(ctx)?;
            controller.attach(target.session.borrow_mut().document_mut());
            controller.bind(ctx, target);
            controller
        }
    };
    for slot in origin.slots.borrow().iter().filter_map(std::rc::Weak::upgrade) {
        if !slot.pending.get() { continue; }
        let Some((_, node)) = mapping.iter().find(|(old, _)| *old == slot.node.get()) else { continue; };
        let mut lease = slot.lease.borrow_mut();
        let Some(lease) = lease.as_mut().filter(|lease| Rc::ptr_eq(&lease.realm, source)) else { continue; };
        let route = Rc::downgrade(&slot);
        let needs_route = {
            let mut slots = destination.slots.borrow_mut();
            slots.retain(|slot| slot.strong_count() != 0);
            let needs_route = !slots.iter().any(|existing| std::rc::Weak::ptr_eq(existing, &route));
            if needs_route {
                slots.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "Details adopted task bookkeeping allocation failed"))?;
            }
            needs_route
        };
        if let Some(task) = slot.task.borrow().as_ref() { task.retarget(&destination.sender)?; }
        lease._node.adopt_nodes(source, target, mapping);
        events::DomEventTarget::from_data(lease._target.clone()).rebind_node(target, *node);
        lease.realm = target.clone();
        lease._document = Some(target.document_value(ctx));
        slot.node.set(*node);
        if needs_route { destination.slots.borrow_mut().push(route); }
    }
    Ok(())
}

fn queue_toggle_task(
    ctx: &mut Ctx,
    realm: Rc<DomRealm>,
    node: NodeId,
    kind: ToggleTaskKind,
    old_state: &'static str,
    new_state: &'static str,
    source: Value,
) -> OpResult<()> {
    let state = match super::realm_services::RealmServices::<RefCell<ToggleTaskState>>::current(ctx)
    {
        Some(state) => state,
        None => super::realm_services::RealmServices::replace_current(
            ctx,
            RefCell::new(ToggleTaskState::default()),
        ),
    };
    let prepared = state.borrow_mut().prepare((node, kind), old_state, new_state, source)
        .map_err(|error| match error {
            PrepareError::SequenceExhausted => OpError::new("QuotaExceededError", "toggle task sequence exhausted"),
            PrepareError::AllocationFailed => OpError::new("QuotaExceededError", "toggle task allocation failed"),
        })?;
    let Prepared::New(id) = prepared else { return Ok(()); };
    let state_for_task = state.clone();
    let queued = scheduling::queue_task(ctx, move |ctx| {
        let task = state_for_task.borrow_mut().take(id);
        let Some(task) = task else {
            return Ok(());
        };
        if realm.session.borrow().document().kind(task.key.0).is_err() {
            return Ok(());
        }
        dispatch_toggle(
            ctx,
            &realm,
            task.key.0,
            "toggle",
            &task.old_state,
            &task.new_state,
            false,
            task.source,
        )?;
        Ok(())
    });
    if let Err(error) = queued {
        let rejected = state.borrow_mut().cancel(id);
        drop(rejected);
        return Err(error);
    }
    Ok(())
}

fn queue_close_event(ctx: &mut Ctx, realm: Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    scheduling::queue_task(ctx, move |ctx| {
        if realm.session.borrow().document().kind(node).is_ok() {
            realm.dispatch_user_agent(ctx, node, "close", false, false, &[])?;
        }
        Ok(())
    })
}

pub(crate) fn popover_value(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<String> {
    let session = realm.session.borrow();
    Ok(
        lumen_html::top_layer::reflected_popover_value(session.document(), node)
            .unwrap_or_default()
            .to_owned(),
    )
}

pub(crate) fn set_popover_value(realm: &Rc<DomRealm>, node: NodeId, value: &str) -> OpResult<()> {
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_attribute_ns(node, None, "popover", value)
        .map_err(super::dom_error)
}

pub(crate) fn show_popover(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let (mut realm, mut node) = realm.resolve_adopted_node(node);
    {
        let session = realm.session.borrow();
        top_layer::validate_show_popover(session.document(), node)
    }
    .map_err(|error| state_error(ctx, error))?;
    if !dispatch_toggle(
        ctx,
        &realm,
        node,
        "beforetoggle",
        "closed",
        "open",
        true,
        Value::Null,
    )? {
        return Ok(());
    }
    (realm, node) = realm.resolve_adopted_node(node);
    let mode_after_callbacks = {
        let session = realm.session.borrow();
        if top_layer::popover_visibility(session.document(), node)
            == top_layer::PopoverVisibility::Showing
        {
            return Ok(());
        }
        top_layer::validate_show_popover(session.document(), node)
    }
    .map_err(|error| state_error(ctx, error))?;
    let before_peers = {
        let session = realm.session.borrow();
        if mode_after_callbacks == top_layer::PopoverMode::Auto {
            top_layer::auto_popovers_to_close(session.document(), node)
        } else {
            Ok(Vec::new())
        }
    }
    .map_err(|error| state_error(ctx, error))?;
    for peer in before_peers {
        let (peer_realm, peer_node) = realm.resolve_adopted_node(peer);
        dispatch_popover_close_beforetoggle(ctx, &peer_realm, peer_node)?;
        let (peer_realm, peer_node) = peer_realm.resolve_adopted_node(peer_node);
        let closed = {
            let mut session = peer_realm.session.borrow_mut();
            if top_layer::popover_visibility(session.document(), peer_node)
                != top_layer::PopoverVisibility::Showing
            {
                false
            } else {
                top_layer::hide_popover_with_state(session.document_mut(), peer_node)
                    .map_err(|error| state_error(ctx, error))?
                    .0
            }
        };
        if closed {
            queue_toggle_task(
                ctx,
                peer_realm,
                peer_node,
                ToggleTaskKind::Popover,
                "open",
                "closed",
                Value::Null,
            )?;
        }
        (realm, node) = realm.resolve_adopted_node(node);
    }
    let mode_after_peer_callbacks = {
        let session = realm.session.borrow();
        top_layer::validate_show_popover(session.document(), node)
    }
    .map_err(|error| state_error(ctx, error))?;
    if mode_after_peer_callbacks != mode_after_callbacks {
        return Err(error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "The popover state changed during beforetoggle",
        ));
    }
    let restore_focus = {
        let session = realm.session.borrow();
        let no_auto_popover = !session.document().top_layer_entries().any(|(_, kind, _)| {
            matches!(
                kind,
                top_layer::Kind::Popover(
                    top_layer::PopoverMode::Auto | top_layer::PopoverMode::Hint
                )
            )
        });
        no_auto_popover.then(|| realm.focused_node()).flatten()
    };
    let transition = top_layer::show_popover_with_focus(
        realm.session.borrow_mut().document_mut(),
        node,
        restore_focus,
    )
    .map_err(|error| state_error(ctx, error))?;
    for peer in transition.closed_auto_popover_nodes {
        queue_toggle_task(
            ctx,
            realm.clone(),
            peer,
            ToggleTaskKind::Popover,
            "open",
            "closed",
            Value::Null,
        )?;
    }
    if transition.changed {
        queue_toggle_task(
            ctx,
            realm.clone(),
            node,
            ToggleTaskKind::Popover,
            "closed",
            "open",
            Value::Null,
        )?;
        realm.focus(ctx, Some(node))?;
    }
    Ok(())
}

pub(crate) fn hide_popover(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let (mut realm, mut node) = realm.resolve_adopted_node(node);
    let showing = {
        let session = realm.session.borrow();
        top_layer::validate_hide_popover(session.document(), node)
    }
    .map_err(|error| state_error(ctx, error))?;
    if !showing {
        return Ok(());
    }
    dispatch_popover_close_beforetoggle(ctx, &realm, node)?;
    (realm, node) = realm.resolve_adopted_node(node);
    let (changed, restore_focus) =
        top_layer::hide_popover_with_state(realm.session.borrow_mut().document_mut(), node)
            .map_err(|error| state_error(ctx, error))?;
    if changed {
        queue_toggle_task(
            ctx,
            realm.clone(),
            node,
            ToggleTaskKind::Popover,
            "open",
            "closed",
            Value::Null,
        )?;
        if let Some(target) =
            restore_focus.filter(|target| realm.session.borrow().document().kind(*target).is_ok())
        {
            realm.focus(ctx, Some(target))?;
        }
    }
    Ok(())
}

pub(crate) fn toggle_popover(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    force: Option<bool>,
) -> OpResult<bool> {
    let (realm, node) = realm.resolve_adopted_node(node);
    let showing = {
        let session = realm.session.borrow();
        top_layer::validate_toggle_popover(session.document(), node)
    };
    let showing = showing.map_err(|error| state_error(ctx, error))?;
    let should_show = force.unwrap_or(!showing);
    if should_show == showing {
        return Ok(showing);
    }
    if should_show {
        show_popover(ctx, &realm, node)?;
    } else {
        hide_popover(ctx, &realm, node)?;
    }
    let (realm, node) = realm.resolve_adopted_node(node);
    let session = realm.session.borrow();
    Ok(top_layer::popover_visibility(session.document(), node)
        == top_layer::PopoverVisibility::Showing)
}

/// Record the pointerdown endpoint for a trusted pointer event. The document
/// owns this single sparse NodeId slot, as required by the light-dismiss
/// algorithm; no popover state changes until a matching pointerup.
pub(crate) fn record_popover_pointerdown_target(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
) -> OpResult<()> {
    let (realm, target) = realm.resolve_adopted_node(target);
    let result =
        top_layer::record_popover_pointerdown_target(realm.session.borrow_mut().document_mut(), target)
            .map_err(|error| state_error(ctx, error));
    result
}

/// Clear the document's pending pointerdown endpoint when the host aborts a
/// pointer sequence without dispatching its matching pointerup.
pub(crate) fn clear_popover_pointerdown_target(realm: &Rc<DomRealm>) {
    top_layer::clear_popover_pointerdown_target(realm.session.borrow_mut().document_mut());
}

/// Apply the pointerup half of popover light dismiss before the trusted event
/// is observable. The core clears and compares the recorded endpoint, then
/// returns a bounded reverse-order close plan. Each candidate is re-resolved
/// and checked after its synchronous `beforetoggle` callback.
pub(crate) fn dismiss_popovers_for_pointer_up(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
) -> OpResult<()> {
    let (realm, target) = realm.resolve_adopted_node(target);
    let plan = {
        let mut session = realm.session.borrow_mut();
        top_layer::popovers_to_hide_on_pointerup(session.document_mut(), target)
    }
    .map_err(|error| state_error(ctx, error))?;
    let Some(plan) = plan else {
        return Ok(());
    };

    for candidate in plan.popovers_to_hide {
        let (popover_realm, popover_node) = realm.resolve_adopted_node(candidate);
        let (endpoint_realm, endpoint) = plan
            .endpoint
            .map(|endpoint| realm.resolve_adopted_node(endpoint))
            .map_or((realm.clone(), None), |(realm, node)| (realm, Some(node)));
        let should_close = if Rc::ptr_eq(&popover_realm, &realm)
            && Rc::ptr_eq(&endpoint_realm, &realm)
        {
            let session = popover_realm.session.borrow();
            top_layer::should_hide_auto_popover_until(session.document(), popover_node, endpoint)
                .map_err(|error| state_error(ctx, error))?
        } else {
            false
        };
        if !should_close {
            continue;
        }

        let _ = dispatch_popover_close_beforetoggle(ctx, &popover_realm, popover_node)?;
        let (popover_realm, popover_node) = popover_realm.resolve_adopted_node(popover_node);
        let (endpoint_realm, endpoint) = plan
            .endpoint
            .map(|endpoint| realm.resolve_adopted_node(endpoint))
            .map_or((realm.clone(), None), |(realm, node)| (realm, Some(node)));
        if !Rc::ptr_eq(&popover_realm, &realm) || !Rc::ptr_eq(&endpoint_realm, &realm) {
            continue;
        }
        let closed = {
            let mut session = popover_realm.session.borrow_mut();
            let should_close = top_layer::should_hide_auto_popover_until(
                session.document(),
                popover_node,
                endpoint,
            )
            .map_err(|error| state_error(ctx, error))?;
            if should_close {
                top_layer::hide_popover(session.document_mut(), popover_node)
                    .map_err(|error| state_error(ctx, error))?
            } else {
                false
            }
        };
        if closed {
            queue_toggle_task(
                ctx,
                popover_realm.clone(),
                popover_node,
                ToggleTaskKind::Popover,
                "open",
                "closed",
                Value::Null,
            )?;
        }
    }
    Ok(())
}

fn dispatch_popover_close_beforetoggle(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
) -> OpResult<bool> {
    let began = {
        let mut session = realm.session.borrow_mut();
        if top_layer::popover_visibility(session.document(), node)
            != top_layer::PopoverVisibility::Showing
        {
            return Ok(false);
        }
        session.document_mut().begin_popover_transition(node)
    };
    if !began {
        return Err(error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "A popover transition is already in progress",
        ));
    }
    let dispatched = dispatch_toggle(
        ctx,
        realm,
        node,
        "beforetoggle",
        "open",
        "closed",
        false,
        Value::Null,
    );
    realm
        .session
        .borrow_mut()
        .document_mut()
        .end_popover_transition(node);
    dispatched
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::{embed::WeakValue, Engine};
    use lumen_runtime::Runtime;

    #[test]
    fn shared_toggle_tracker_keeps_latest_source_and_trusted_equal_states() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(),
            "<body><dialog id='target'></dialog></body>",32).unwrap();
        boolean(&mut engine,r#"
            globalThis.sources = [document.createElement('button'), document.createElement('button')];
            globalThis.transitions = [];
            document.getElementById('target').addEventListener('toggle', event =>
                transitions.push([event.oldState,event.newState,event.source,event.isTrusted,event instanceof ToggleEvent]));
            true
        "#);
        let first = eval_value(&mut engine,"sources[0]");
        let latest = eval_value(&mut engine,"sources[1]");
        let node = realm.with_session(|session| selector::query_selector(session.document(),
            session.document().root(),"#target").unwrap().unwrap());
        queue_toggle_task(engine.ctx(),realm.clone(),node,ToggleTaskKind::Dialog,
            "closed","open",first).unwrap();
        queue_toggle_task(engine.ctx(),realm,node,ToggleTaskKind::Dialog,
            "open","closed",latest).unwrap();
        assert!(scheduling::run_tasks(&mut engine,16).is_empty());
        boolean(&mut engine,"transitions.length===1 && transitions[0][0]==='closed' && transitions[0][1]==='closed' && transitions[0][2]===sources[1] && transitions[0][3] && transitions[0][4]");
    }

    #[test]
    fn shared_toggle_tracker_rolls_back_real_task_admission_failure() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(),
            "<body><dialog id='target'></dialog></body>",32).unwrap();
        boolean(&mut engine,"globalThis.transitions=[];document.getElementById('target').addEventListener('toggle',event=>transitions.push(event.oldState+'/'+event.newState));true");
        let node = realm.with_session(|session| selector::query_selector(session.document(),
            session.document().root(),"#target").unwrap().unwrap());
        let unrelated = Rc::new(std::cell::Cell::new(0));
        let callback_count = unrelated.clone();
        scheduling::queue_task(engine.ctx(),move |_| { callback_count.set(callback_count.get()+1);Ok(()) }).unwrap();
        for _ in 1..scheduling::MAX_PENDING_HTML_TASKS {
            scheduling::queue_task(engine.ctx(),|_|Ok(())).unwrap();
        }
        let error = queue_toggle_task(engine.ctx(),realm.clone(),node,ToggleTaskKind::Dialog,
            "closed","open",Value::Null).unwrap_err();
        assert_eq!(error.class(),"QuotaExceededError");
        let state = super::super::realm_services::RealmServices::<RefCell<ToggleTaskState>>::current(engine.ctx()).unwrap();
        assert!(state.borrow().is_empty());
        assert!(scheduling::run_tasks(&mut engine,scheduling::MAX_PENDING_HTML_TASKS).is_empty());
        assert_eq!(unrelated.get(),1);
        boolean(&mut engine,"transitions.length===0");
        queue_toggle_task(engine.ctx(),realm,node,ToggleTaskKind::Dialog,
            "closed","open",Value::Null).unwrap();
        assert!(scheduling::run_tasks(&mut engine,16).is_empty());
        assert!(state.borrow().is_empty());
        boolean(&mut engine,"transitions.join(',')==='closed/open'");
    }

    fn boolean(engine: &mut Engine, source: &str) {
        assert!(matches!(
            eval_value(engine, source),
            lumen::embed::Value::Bool(true)
        ));
    }

    fn eval_value(engine: &mut Engine, source: &str) -> lumen::embed::Value {
        match engine.eval_value(source) {
            Ok(Ok(value)) => value,
            Ok(Err(thrown)) => match engine.describe_throw(thrown) {
                lumen::Completion::Throw { name, message } => {
                    panic!("dialog/popover script threw {name}: {message}")
                }
                lumen::Completion::Value(message) => {
                    panic!("dialog/popover script threw: {message}")
                }
            },
            Err(error) => panic!("dialog/popover script failed to parse: {error:?}"),
        }
    }

    fn weak_global(engine: &mut Engine, name: &str) -> WeakValue {
        let global = engine.ctx().global_object();
        let value = engine
            .ctx()
            .member_get(&global, name)
            .ok()
            .expect("global test value exists");
        engine
            .ctx()
            .weak_value(&value)
            .expect("test value is an object")
    }

    fn engine() -> Engine {
        let mut engine = Engine::new();
        super::super::install(
            engine.ctx(),
            "<html><body><dialog></dialog></body></html>",
            32,
        )
        .unwrap();
        engine
    }

    #[test]
    fn details_detached_toggle_retains_target_and_coalesces_after_gc() {
        let mut engine = engine();
        boolean(&mut engine, r#"(() => {
            globalThis.detailsEvents = [];
            const d = document.createElement('details');
            globalThis.detailsProbe = d;
            d.ontoggle = function(e) {
                detailsEvents.push(e instanceof ToggleEvent && e.isTrusted &&
                    !e.bubbles && !e.cancelable && e.source === null &&
                    e.target === this && e.currentTarget === this &&
                    e.oldState === 'closed' && e.newState === 'open');
            };
            d.open = true; d.removeAttribute('open'); d.setAttribute('open', '');
            return detailsEvents.length === 0;
        })()"#);
        let target = weak_global(&mut engine, "detailsProbe");
        boolean(&mut engine, "detailsProbe = null; true");
        engine.collect_garbage();
        assert!(target.upgrade().is_some(), "queued task retains detached target");
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "detailsEvents.length === 1 && detailsEvents[0]");
        engine.collect_garbage();
        assert!(target.upgrade().is_none(), "completed task releases detached target");
    }

    #[test]
    fn details_reentrant_toggle_waits_for_a_later_task_turn() {
        let mut engine = engine();
        boolean(&mut engine, r#"(() => {
            globalThis.detailsTurns = [];
            const d = document.createElement('details');
            d.ontoggle = function(e) {
                detailsTurns.push(e.oldState + '/' + e.newState);
                if (detailsTurns.length === 1) this.open = false;
            };
            d.open = true;
            return detailsTurns.length === 0;
        })()"#);
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "detailsTurns.join(',') === 'closed/open'");
        engine.collect_garbage();
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "detailsTurns.join(',') === 'closed/open,open/closed'");
    }

    #[test]
    fn details_initial_html_and_xhtml_attributes_admit_real_tasks() {
        for xml in [false, true] {
            let mut engine = Engine::new();
            let source = "<html xmlns='http://www.w3.org/1999/xhtml'><body><details open='' /></body></html>";
            if xml { super::super::install_xhtml(engine.ctx(), source, 32).unwrap(); }
            else { super::super::install(engine.ctx(), source, 32).unwrap(); }
            boolean(&mut engine, r#"(() => {
                globalThis.initialDetails = [];
                document.querySelector('details').ontoggle = e =>
                    initialDetails.push(e.isTrusted && e.oldState === 'closed' && e.newState === 'open');
                return initialDetails.length === 0;
            })()"#);
            assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
            boolean(&mut engine, "initialDetails.length === 1 && initialDetails[0]");
        }
    }

    #[test]
    fn details_adoption_before_delivery_preserves_native_target_and_handler() {
        let mut engine = engine();
        boolean(&mut engine, r#"(() => {
            globalThis.adoptedDetailsResult = false;
            const d = document.createElement('details');
            const doc = document.implementation.createHTMLDocument();
            d.ontoggle = function(e) {
                adoptedDetailsResult = e.target === this && this.ownerDocument === doc &&
                    e.isTrusted && e.oldState === 'closed' && e.newState === 'open';
            };
            d.open = true;
            doc.adoptNode(d);
            return !adoptedDetailsResult;
        })()"#);
        engine.collect_garbage();
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "adoptedDetailsResult");
    }

    #[test]
    fn details_multiple_adoptions_coalesce_into_original_task_position() {
        let mut engine = engine();
        boolean(&mut engine, r#"(() => {
            globalThis.adoptionOrder = [];
            const d = document.createElement('details');
            const a = document.implementation.createHTMLDocument();
            const b = document.implementation.createHTMLDocument();
            d.ontoggle = function(e) {
                adoptionOrder.push(e.target === d && d.ownerDocument === b &&
                    e.oldState === 'closed' && e.newState === 'closed' && e.isTrusted);
            };
            d.open = true;
            a.adoptNode(d);
            d.open = false;
            b.adoptNode(d);
            for (let i = 0; i < 64; ++i) { a.adoptNode(d); b.adoptNode(d); }
            d.open = true;
            d.open = false;
            const peer = document.createElement('details');
            peer.ontoggle = () => adoptionOrder.push('peer');
            peer.open = true;
            return adoptionOrder.length === 0;
        })()"#);
        engine.collect_garbage();
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "adoptionOrder.length === 2 && adoptionOrder[0] === true && adoptionOrder[1] === 'peer'");
    }

    #[test]
    fn details_real_quota_failure_preserves_mutation_and_reports_outside_borrow() {
        let mut engine = engine();
        for _ in 0..scheduling::MAX_PENDING_HTML_TASKS {
            scheduling::queue_task(engine.ctx(), |_| Ok(())).unwrap();
        }
        boolean(&mut engine, r#"(() => {
            globalThis.quotaDetails = document.createElement('details');
            globalThis.quotaCalls = 0;
            quotaDetails.ontoggle = () => quotaCalls++;
            quotaDetails.open = true;
            return quotaDetails.open && quotaCalls === 0;
        })()"#);
        let errors = scheduling::run_tasks(&mut engine, scheduling::MAX_PENDING_HTML_TASKS);
        assert_eq!(errors.len(), 1, "rejected native admission reports concretely");
        boolean(&mut engine, "quotaCalls === 0");
        boolean(&mut engine, "quotaDetails.open = false; true");
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "quotaCalls === 1");
    }

    #[test]
    fn toggle_event_exposes_old_and_new_state_as_a_native_interface() {
        let mut engine = engine();
        let result = engine.eval_value("let e = new ToggleEvent('toggle', {oldState:'closed', newState:'open'}); e instanceof Event && e instanceof ToggleEvent && e.oldState === 'closed' && e.newState === 'open' && e.source === null");
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }

    #[test]
    fn event_related_target_is_only_exposed_by_interfaces_that_define_it() {
        let mut engine = engine();
        boolean(
            &mut engine,
            r#"(() => {
                const event = new Event('test');
                const toggle = new ToggleEvent('toggle');
                const target = document.body;
                const focus = new FocusEvent('focus', {relatedTarget: target});
                const mouse = new MouseEvent('click', {relatedTarget: target});
                return event.relatedTarget === undefined && !('relatedTarget' in event) &&
                    toggle.relatedTarget === undefined && !('relatedTarget' in toggle) &&
                    focus.relatedTarget === target && mouse.relatedTarget === target;
            })()"#,
        );
    }

    #[test]
    fn closed_dialog_and_popover_rendering_defaults_follow_top_layer_state() {
        let mut engine = engine();
        boolean(
            &mut engine,
            r#"(() => {
                const dialog = document.querySelector('dialog');
                const popover = document.createElement('div');
                popover.popover = 'manual';
                document.body.append(popover);
                if (getComputedStyle(dialog).display !== 'none' ||
                    getComputedStyle(popover).display !== 'none') return false;
                dialog.show();
                if (getComputedStyle(dialog).display === 'none') return false;
                popover.showPopover();
                if (getComputedStyle(popover).display === 'none') return false;
                popover.hidePopover();
                return getComputedStyle(popover).display === 'none';
            })()"#,
        );
    }

    #[test]
    fn modal_dialogs_require_an_active_browsing_context() {
        let mut engine = engine();
        boolean(
            &mut engine,
            r#"(() => {
                const detached = document.implementation.createHTMLDocument();
                const dialog = detached.createElement('dialog');
                detached.body.append(dialog);
                try {
                    dialog.showModal();
                } catch (error) {
                    return error.name === 'InvalidStateError' && !dialog.open;
                }
                return false;
            })()"#,
        );
    }

    #[test]
    fn dialog_and_popover_toggle_tasks_use_their_own_trackers() {
        let mut engine = engine();
        boolean(
            &mut engine,
            r#"(() => {
                const dialog = document.createElement('dialog');
                dialog.popover = 'manual';
                document.body.append(dialog);
                globalThis.topLayerTransitions = [];
                dialog.addEventListener('toggle', event =>
                    topLayerTransitions.push(event.oldState + '/' + event.newState));
                dialog.show();
                dialog.showPopover();
                return dialog.open && dialog.matches(':popover-open');
            })()"#,
        );
        assert!(super::scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(
            &mut engine,
            "topLayerTransitions.length === 2 && topLayerTransitions.every(state => state === 'closed/open')",
        );
    }

    #[test]
    fn showing_manual_popover_preserves_unrelated_auto_popovers() {
        let mut engine = engine();
        boolean(
            &mut engine,
            r#"(() => {
                const auto = document.createElement('div');
                const manual = document.createElement('div');
                auto.popover = 'auto';
                manual.popover = 'manual';
                document.body.append(auto, manual);
                auto.showPopover();
                manual.showPopover();
                return auto.matches(':popover-open') && manual.matches(':popover-open');
            })()"#,
        );
    }

    #[test]
    fn toggle_event_source_slot_traces_source_without_rooting_cycles() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        super::super::install(engine.ctx(), "<div></div>", 64).unwrap();
        boolean(
            engine,
            r#"(() => {
                const source = document.createElement('div');
                const event = new ToggleEvent('toggle', {source});
                source.event = event;
                globalThis.toggleSourceNode = source;
                globalThis.toggleSourceEvent = event;
                return event.source === source;
            })()"#,
        );

        let (event_weak, source_weak): (WeakValue, WeakValue) = {
            let global = engine.ctx().global_object();
            let event = engine
                .ctx()
                .member_get(&global, "toggleSourceEvent")
                .ok()
                .expect("ToggleEvent wrapper exists");
            let source = engine
                .ctx()
                .member_get(&event, "source")
                .ok()
                .expect("ToggleEvent source getter succeeds");
            assert!(matches!(
                engine
                    .ctx()
                    .with_instance::<DomToggleEvent, _>(&event, |event| {
                        event.source_slot.is_some()
                    }),
                Ok(true)
            ));
            (
                engine.ctx().weak_value(&event).expect("event is an object"),
                engine
                    .ctx()
                    .weak_value(&source)
                    .expect("source is an object"),
            )
        };

        engine.collect_garbage();
        assert!(event_weak.upgrade().is_some(), "global event remains live");
        assert!(
            source_weak.upgrade().is_some(),
            "private source slot is traced"
        );
        boolean(
            engine,
            "toggleSourceEvent = null; toggleSourceNode = null; true",
        );
        engine.collect_garbage();
        let event_alive = event_weak.upgrade().is_some();
        let source_alive = source_weak.upgrade().is_some();
        assert!(
            !event_alive,
            "event/source cycle is collectible (event_alive={event_alive}, source_alive={source_alive})"
        );
        assert!(
            !source_alive,
            "source/event cycle is collectible (event_alive={event_alive}, source_alive={source_alive})"
        );
    }

    #[test]
    fn native_identity_owner_traces_connected_shadow_template_and_attribute_wrappers() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<html><body></body></html>", 128).unwrap();
        boolean(
            &mut engine,
            r#"(() => {
                const host = document.createElement('div');
                host.id = 'identity-host';
                host.setAttribute('data-identity', 'present');
                const attr = host.getAttributeNode('data-identity');
                attr.identityMarker = 11;
                document.documentElement.appendChild(host);

                const light = document.createElement('span');
                light.id = 'identity-light';
                light.identityMarker = 22;
                host.appendChild(light);

                const closed = host.attachShadow({mode:'closed'});
                const hidden = document.createElement('i');
                hidden.identityMarker = 33;
                closed.appendChild(hidden);

                const template = document.createElement('template');
                const templated = document.createElement('b');
                templated.identityMarker = 44;
                template.content.appendChild(templated);
                document.documentElement.appendChild(template);

                globalThis.identityAttr = attr;
                globalThis.identityLight = light;
                globalThis.identityClosed = hidden;
                globalThis.identityTemplate = templated;
                return true;
            })()"#,
        );
        let attr = weak_global(&mut engine, "identityAttr");
        let light = weak_global(&mut engine, "identityLight");
        let closed = weak_global(&mut engine, "identityClosed");
        let template = weak_global(&mut engine, "identityTemplate");
        boolean(
            &mut engine,
            "identityAttr = null; identityLight = null; identityClosed = null; identityTemplate = null; true",
        );

        engine.collect_garbage();
        assert!(
            attr.upgrade().is_some(),
            "connected Attr wrapper stays canonical"
        );
        assert!(
            light.upgrade().is_some(),
            "connected light child wrapper stays canonical"
        );
        assert!(
            closed.upgrade().is_some(),
            "closed-shadow child wrapper stays canonical"
        );
        assert!(
            template.upgrade().is_some(),
            "template-content child wrapper stays canonical"
        );
        boolean(
            &mut engine,
            r#"document.getElementById('identity-light').identityMarker === 22 &&
                document.getElementById('identity-host').getAttributeNode('data-identity').identityMarker === 11 &&
                document.querySelector('template').content.firstChild.identityMarker === 44"#,
        );
        let closed = closed.upgrade().expect("closed-shadow wrapper is traced");
        let marker = engine
            .ctx()
            .member_get(&closed, "identityMarker")
            .ok()
            .expect("closed-shadow expando remains available");
        assert!(matches!(marker, lumen::embed::Value::Num(value) if value == 33.0));
    }

    #[test]
    fn native_identity_owner_traces_detached_components_from_parent_or_child_and_owner_document() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<html><body></body></html>", 192).unwrap();
        boolean(
            &mut engine,
            r#"(() => {
                const parent = document.createElement('div');
                parent.setAttribute('data-identity', 'detached');
                const attr = parent.getAttributeNode('data-identity');
                attr.identityMarker = 51;
                const first = document.createElement('i');
                first.identityMarker = 52;
                const child = document.createElement('b');
                child.identityMarker = 53;
                const last = document.createElement('u');
                last.identityMarker = 54;
                parent.appendChild(first);
                parent.appendChild(child);
                parent.appendChild(last);
                globalThis.identityDetachedParent = parent;
                globalThis.identityDetachedAttr = attr;
                globalThis.identityDetachedChild = child;
                globalThis.identityDetachedFirst = first;
                globalThis.identityDetachedLast = last;
                return true;
            })()"#,
        );
        let parent = weak_global(&mut engine, "identityDetachedParent");
        let attr = weak_global(&mut engine, "identityDetachedAttr");
        let child = weak_global(&mut engine, "identityDetachedChild");
        let first = weak_global(&mut engine, "identityDetachedFirst");
        let last = weak_global(&mut engine, "identityDetachedLast");
        boolean(
            &mut engine,
            "identityDetachedAttr = null; identityDetachedChild = null; identityDetachedFirst = null; identityDetachedLast = null; true",
        );
        engine.collect_garbage();
        assert!(parent.upgrade().is_some());
        assert!(
            attr.upgrade().is_some(),
            "detached owner's Attr identity is traced"
        );
        assert!(
            child.upgrade().is_some(),
            "detached parent traces descendants"
        );
        assert!(first.upgrade().is_some());
        assert!(last.upgrade().is_some());
        boolean(
            &mut engine,
            r#"identityDetachedParent.firstChild.identityMarker === 52 &&
                identityDetachedParent.firstChild.nextSibling.identityMarker === 53 &&
                identityDetachedParent.lastChild.identityMarker === 54 &&
                identityDetachedParent.getAttributeNode('data-identity').identityMarker === 51"#,
        );

        boolean(
            &mut engine,
            r#"(() => {
                const parent = document.createElement('section');
                const previous = document.createElement('i');
                previous.identityMarker = 61;
                const child = document.createElement('b');
                child.identityMarker = 62;
                const next = document.createElement('u');
                next.identityMarker = 63;
                parent.appendChild(previous);
                parent.appendChild(child);
                parent.appendChild(next);
                globalThis.identityChildRoot = child;
                globalThis.identitySiblingParent = parent;
                globalThis.identitySiblingBefore = previous;
                globalThis.identitySiblingAfter = next;
                return true;
            })()"#,
        );
        let sibling_parent = weak_global(&mut engine, "identitySiblingParent");
        let sibling_before = weak_global(&mut engine, "identitySiblingBefore");
        let sibling_after = weak_global(&mut engine, "identitySiblingAfter");
        let child_root = weak_global(&mut engine, "identityChildRoot");
        boolean(
            &mut engine,
            "identitySiblingParent = null; identitySiblingBefore = null; identitySiblingAfter = null; true",
        );
        engine.collect_garbage();
        assert!(
            sibling_parent.upgrade().is_some(),
            "child traces its detached parent"
        );
        assert!(
            sibling_before.upgrade().is_some(),
            "child traces preceding sibling"
        );
        assert!(
            sibling_after.upgrade().is_some(),
            "child traces following sibling"
        );
        boolean(
            &mut engine,
            r#"identityChildRoot.parentNode.firstChild.identityMarker === 61 &&
                identityChildRoot.parentNode.lastChild.identityMarker === 63"#,
        );

        boolean(
            &mut engine,
            r#"(() => {
                const ownedDocument = document.implementation.createHTMLDocument('owned');
                ownedDocument.identityMarker = 71;
                const ownedNode = ownedDocument.createElement('p');
                ownedDocument.body.appendChild(ownedNode);
                globalThis.identityOwnerDocumentNode = ownedNode;
                globalThis.identityOwnerDocument = ownedDocument;
                return true;
            })()"#,
        );
        let owner_document = weak_global(&mut engine, "identityOwnerDocument");
        boolean(&mut engine, "identityOwnerDocument = null; true");
        engine.collect_garbage();
        assert!(
            owner_document.upgrade().is_some(),
            "live Node traces ownerDocument"
        );
        boolean(
            &mut engine,
            "identityOwnerDocumentNode.ownerDocument.identityMarker === 71",
        );

        // Drop every script root to these detached trees. Their expando-bearing
        // wrappers are no longer retained merely because their identities exist.
        boolean(
            &mut engine,
            "identityDetachedParent = null; identityChildRoot = null; identityOwnerDocumentNode = null; true",
        );
        engine.collect_garbage();
        assert!(
            parent.upgrade().is_none(),
            "unreachable detached parent is collected"
        );
        assert!(
            child_root.upgrade().is_none(),
            "unreachable detached child is collected"
        );
        assert!(
            owner_document.upgrade().is_none(),
            "unreachable detached ownerDocument is collected"
        );
    }

    #[test]
    fn native_identity_owner_pins_are_scoped_to_the_collecting_engine() {
        let mut engine_a = engine();
        let mut engine_b = engine();
        boolean(
            &mut engine_b,
            r#"(() => {
                const parent = document.createElement('div');
                const child = document.createElement('span');
                child.identityMarker = 81;
                parent.appendChild(child);
                globalThis.crossEngineParent = parent;
                globalThis.crossEngineChild = child;

                const source = document.createElement('div');
                const event = new ToggleEvent('toggle', {source});
                source.event = event;
                globalThis.crossEngineSource = source;
                globalThis.crossEngineEvent = event;
                return true;
            })()"#,
        );
        let child = weak_global(&mut engine_b, "crossEngineChild");
        let source = weak_global(&mut engine_b, "crossEngineSource");
        let event = weak_global(&mut engine_b, "crossEngineEvent");
        boolean(&mut engine_b, "crossEngineChild = null; true");

        engine_a.collect_garbage();
        assert!(
            child.upgrade().is_some(),
            "another engine GC preserves a rooted DOM component"
        );
        assert!(
            source.upgrade().is_some(),
            "another engine GC treats the owner pin as external"
        );
        assert!(
            event.upgrade().is_some(),
            "another engine GC preserves its traced event edge"
        );
        boolean(
            &mut engine_b,
            "crossEngineParent.firstChild.identityMarker === 81",
        );

        boolean(
            &mut engine_b,
            "crossEngineSource = null; crossEngineEvent = null; true",
        );
        engine_a.collect_garbage();
        assert!(
            source.upgrade().is_some(),
            "only the owning engine may collect the cycle"
        );
        assert!(
            event.upgrade().is_some(),
            "the non-owning collection preserves the cycle"
        );
        engine_b.collect_garbage();
        assert!(
            source.upgrade().is_none(),
            "the owner collects an unrooted source/event cycle"
        );
        assert!(
            event.upgrade().is_none(),
            "the owner collects an unrooted event/source cycle"
        );
        assert!(
            child.upgrade().is_some(),
            "the detached parent still owns its child identity"
        );
        boolean(&mut engine_b, "crossEngineParent = null; true");
        engine_b.collect_garbage();
        assert!(
            child.upgrade().is_none(),
            "the detached component collects after its root is released"
        );
    }

    #[test]
    fn toggle_event_source_is_retargeted_after_shadow_dispatch() {
        let mut engine = engine();
        let result = eval_value(
            &mut engine,
            r#"(() => {
                const host = document.documentElement.appendChild(document.createElement('div'));
                const root = host.attachShadow({mode: 'open'});
                const source = document.createElement('button');
                const target = document.createElement('div');
                root.appendChild(source);
                root.appendChild(target);
                let targetSawSource = false;
                let hostSawRetargetedSource = false;
                target.addEventListener('toggle', event => {
                    targetSawSource = event.source === source;
                });
                host.addEventListener('toggle', event => {
                    hostSawRetargetedSource = event.source === host;
                });
                const event = new ToggleEvent('toggle', {source, bubbles:true, composed:true});
                target.dispatchEvent(event);
                const retargeted = {
                    targetSawSource,
                    hostSawRetargetedSource,
                    afterDispatchSawHost: event.source === host,
                    finalTargetIsHost: event.target === host
                };
                host.remove();
                return JSON.stringify(retargeted);
            })()"#,
        );
        assert_eq!(
            engine
                .ctx()
                .coerce_string(&result)
                .ok()
                .expect("diagnostic is a string")
                .as_ref(),
            r#"{"targetSawSource":true,"hostSawRetargetedSource":true,"afterDispatchSawHost":true,"finalTargetIsHost":true}"#,
            "ToggleEvent.source must follow the dispatch retargeting context"
        );
    }

    #[test]
    fn dialog_focus_steps_choose_autofocus_then_first_focusable_and_dialog_fallback() {
        let mut engine = engine();
        let result = engine.eval_value(
            r#"(() => {
                const autoDialog = document.createElement('dialog');
                const hidden = document.createElement('input');
                hidden.type = 'hidden';
                hidden.setAttribute('autofocus', '');
                const autofocus = document.createElement('button');
                autofocus.setAttribute('autofocus', '');
                const later = document.createElement('button');
                autoDialog.append(hidden, autofocus, later);
                document.body.append(autoDialog);
                autoDialog.show();
                const choseAutofocus = document.activeElement === autofocus;
                autoDialog.close();

                const firstDialog = document.createElement('dialog');
                const first = document.createElement('button');
                firstDialog.append(first);
                document.body.append(firstDialog);
                firstDialog.showModal();
                const choseFirstFocusable = document.activeElement === first;
                firstDialog.close();

                const emptyDialog = document.createElement('dialog');
                document.body.append(emptyDialog);
                emptyDialog.show();
                const choseModelessDialog = document.activeElement === emptyDialog;
                emptyDialog.close();

                const emptyModalDialog = document.createElement('dialog');
                document.body.append(emptyModalDialog);
                emptyModalDialog.showModal();
                const choseModalDialog = document.activeElement === emptyModalDialog;
                emptyModalDialog.close();
                return choseAutofocus && choseFirstFocusable &&
                    choseModelessDialog && choseModalDialog;
            })()"#,
        );
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }

    #[test]
    fn dialog_show_rebinds_its_receiver_after_beforetoggle_adoption() {
        let mut engine = engine();
        let result = engine.eval_value(
            r#"(() => {
                const dialog = document.createElement('dialog');
                document.body.append(dialog);
                const other = document.implementation.createHTMLDocument('other');
                dialog.addEventListener('beforetoggle', () => {
                    other.body.append(other.adoptNode(dialog));
                }, {once: true});
                dialog.show();
                return dialog.ownerDocument === other && dialog.open &&
                    other.getElementsByTagName('dialog')[0] === dialog;
            })()"#,
        );
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }

    #[test]
    fn toggle_event_converts_inherited_then_derived_dictionary_order_and_validates_source() {
        let mut engine = engine();
        let result = engine.eval_value(
            r#"(() => {
                const order = [];
                const source = document.createElement('div');
                const values = {
                    bubbles: true, cancelable: false, composed: true,
                    newState: 'open', oldState: 'closed', source
                };
                const event = new ToggleEvent('toggle', new Proxy({}, {
                    get(_target, key) { order.push(String(key)); return values[key]; }
                }));
                if (order.join(',') !== 'bubbles,cancelable,composed,newState,oldState,source' ||
                    event.source !== source || event.oldState !== 'closed' || event.newState !== 'open')
                    return false;

                const invalidOrder = [];
                let rejected = false;
                try {
                    new ToggleEvent('toggle', new Proxy({}, {
                        get(_target, key) {
                            invalidOrder.push(String(key));
                            return key === 'source' ? {} : undefined;
                        }
                    }));
                } catch (error) { rejected = error instanceof TypeError; }
                return rejected &&
                    invalidOrder.join(',') === 'bubbles,cancelable,composed,newState,oldState,source';
            })()"#,
        );
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }

    #[test]
    fn dialog_toggle_tasks_coalesce_close_then_show_without_dropping_equal_states() {
        let mut engine = engine();
        let result = engine.eval_value(
            r#"(() => {
                const dialog = document.querySelector('dialog');
                globalThis.dialogToggleStates = [];
                dialog.addEventListener('toggle', event => {
                    dialogToggleStates.push(event.oldState + '/' + event.newState);
                });
                dialog.show();
                return dialog.open;
            })()"#,
        );
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
        assert!(super::scheduling::run_tasks(&mut engine, 16).is_empty());
        boolean(&mut engine, "dialogToggleStates.length = 0; document.querySelector('dialog').close(); document.querySelector('dialog').show(); true");
        assert!(super::scheduling::run_tasks(&mut engine, 16).is_empty());
        let result = engine.eval_value("dialogToggleStates.join(',') === 'open/open'");
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }

    #[test]
    fn dialog_close_preserves_omitted_values_and_sets_result_after_beforetoggle() {
        let mut engine = engine();
        let result = engine.eval_value(
            r#"(() => {
                const dialog = document.querySelector('dialog');
                dialog.returnValue = 'before';
                dialog.show();
                let observed;
                dialog.addEventListener('beforetoggle', () => {
                    observed = dialog.returnValue;
                }, {once: true});
                dialog.close('after');
                if (observed !== 'before' || dialog.returnValue !== 'after') return false;

                dialog.returnValue = 'omitted';
                dialog.show();
                dialog.close();
                if (dialog.returnValue !== 'omitted') return false;

                dialog.returnValue = 'undefined';
                dialog.show();
                dialog.close(undefined);
                if (dialog.returnValue !== 'undefined') return false;

                dialog.show();
                dialog.close(null);
                if (dialog.returnValue !== 'null') return false;

                dialog.show();
                dialog.close('');
                if (dialog.returnValue !== '') return false;

                dialog.returnValue = 'closed';
                dialog.close('no-op');
                return dialog.returnValue === 'closed';
            })()"#,
        );
        assert!(matches!(result, Ok(Ok(lumen::embed::Value::Bool(true)))));
    }
}
