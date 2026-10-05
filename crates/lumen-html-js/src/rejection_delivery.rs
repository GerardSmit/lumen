//! Browser promise-rejection delivery shared by native Window runtimes, the host WPT profile and
//! worker globals. Node's fatal rejection policy lives elsewhere and is never mixed in here.
//!
//! The policy collects `unhandledrejection` / `rejectionhandled` notifications at genuine
//! checkpoints, keeps the creating realm and the original reason with each one, and lets each host
//! admit them on its own task source through a registered sink.
use lumen::embed::{Ctx, RealmHandle, Value, WeakValue};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
};

use crate::DomRealm;

/// A promise rejection notification collected at a runtime checkpoint for a browser host.
pub enum BrowserRejectionEvent {
    Unhandled {
        batch: BrowserRejectionBatch,
        owner: RealmHandle,
        promise: Value,
        reason: Value,
    },
    Handled {
        owner: RealmHandle,
        promise: Value,
        reason: Value,
    },
}

impl BrowserRejectionEvent {
    pub fn owner(&self) -> &RealmHandle {
        match self {
            Self::Unhandled { owner, .. } | Self::Handled { owner, .. } => owner,
        }
    }
}

/// Identity of one cloned checkpoint notification list. Hosts preserve this task boundary.
#[derive(Clone)]
pub struct BrowserRejectionBatch(Rc<()>);

impl BrowserRejectionBatch {
    pub fn same_batch(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

pub type BrowserRejectionSink = Rc<dyn Fn(&mut Ctx, Vec<BrowserRejectionEvent>, BrowserRejectionDelivery) -> Vec<Value>>;
type BrowserRejectionSinks = Rc<RefCell<HashMap<usize, (WeakValue, BrowserRejectionSink)>>>;
type PendingRejections = Rc<RefCell<VecDeque<BrowserRejectionEvent>>>;

fn browser_realm_key(engine: &mut lumen::Engine, realm: &RealmHandle) -> Option<usize> {
    let key = engine.ctx().object_addr(&realm.global())?;
    engine.host_realm_for_key(key).filter(|registered| registered.same_realm(realm)).map(|_| key)
}

fn browser_realm_key_ctx(ctx: &mut Ctx, realm: &RealmHandle) -> Option<usize> {
    let key = ctx.object_addr(&realm.global())?;
    ctx.host_realm_for_key(key).filter(|registered| registered.same_realm(realm)).map(|_| key)
}

#[derive(Default)]
pub(crate) struct BrowserAdmissionErrors {
    pub(crate) first: Option<Value>,
    pub(crate) count: u64,
    pub(crate) overflow: bool,
}

pub(crate) fn record_browser_admission_errors(state: &Rc<RefCell<BrowserAdmissionErrors>>, mut errors: Vec<Value>) {
    if errors.is_empty() {
        return;
    }
    let count = errors.len() as u64;
    let mut first = Some(errors.remove(0));
    {
        let mut state = state.borrow_mut();
        if state.first.is_none() {
            state.first = first.take();
        }
        match state.count.checked_add(count) {
            Some(count) => state.count = count,
            None => {
                state.count = u64::MAX;
                state.overflow = true;
            }
        }
    }
    drop(first);
    drop(errors);
}

pub(crate) fn drain_browser_admission_errors(ctx: &mut Ctx, state: &Rc<RefCell<BrowserAdmissionErrors>>) -> Vec<Value> {
    let errors = std::mem::take(&mut *state.borrow_mut());
    let mut result = Vec::new();
    if let Some(first) = errors.first {
        result.push(first);
    }
    if errors.count > 1 || errors.overflow {
        let qualifier = if errors.overflow { "at least " } else { "" };
        result.push(
            lumen::embed::OpError::new(
                "Error",
                format!(
                    "{qualifier}{} additional browser rejection task admission failures",
                    errors.count.saturating_sub(1)
                ),
            )
            .to_value(ctx),
        );
    }
    result
}

fn collect_browser_handled(ctx: &mut Ctx, promise: Value, delivery: &BrowserRejectionDelivery) -> Option<BrowserRejectionEvent> {
    let identity = ctx.object_addr(&promise)?;
    let record = delivery.0.borrow_mut().records.remove(&identity)?;
    let still_same_promise = record.promise.upgrade().and_then(|tracked| ctx.object_addr(&tracked)) == Some(identity);
    if !record.delivered || !still_same_promise || !delivery.0.borrow_mut().realm_is_live(record.realm_key) {
        return None;
    }
    let owner = ctx.host_realm_for_key(record.realm_key)?;
    let reason = ctx.promise_rejection_reason(&promise)?;
    Some(BrowserRejectionEvent::Handled { owner, promise, reason })
}

fn collect_browser_rejections(engine: &mut lumen::Engine, delivery: &BrowserRejectionDelivery, pending: &PendingRejections) {
    // The host receives an actual UA task in the Promise's creating realm. Preserve only a weak
    // Promise identity between notifications so an unhandled Promise does not by itself keep a
    // retired document alive.
    for promise in engine.take_late_handled_rejections() {
        if let Some(event) = collect_browser_handled(engine.ctx(), promise, delivery) {
            pending.borrow_mut().push_back(event);
        }
    }
    delivery.0.borrow_mut().records.retain(|_, record| record.promise.upgrade().is_some());
    let mut batch = None;
    for (owner, promise, reason) in engine.take_unhandled_rejections_with_realm() {
        let Some(realm_key) = browser_realm_key(engine, &owner) else {
            continue;
        };
        if !delivery.0.borrow_mut().realm_is_live(realm_key) {
            engine.discard_rejections_for_realm(&owner);
            continue;
        }
        let Some(identity) = engine.ctx().object_addr(&promise) else {
            continue;
        };
        let Some(weak_promise) = engine.ctx().weak_value(&promise) else {
            continue;
        };
        let mut tracking = delivery.0.borrow_mut();
        if !tracking.records.contains_key(&identity) {
            tracking.records.insert(
                identity,
                BrowserRejectionRecord { realm_key, promise: weak_promise, delivered: false },
            );
            pending.borrow_mut().push_back(BrowserRejectionEvent::Unhandled {
                batch: batch.get_or_insert_with(|| BrowserRejectionBatch(Rc::new(()))).clone(),
                owner,
                promise,
                reason,
            });
        }
    }
}

fn admit_browser_rejections(
    ctx: &mut Ctx,
    delivery: &BrowserRejectionDelivery,
    pending: &PendingRejections,
    sinks: &BrowserRejectionSinks,
) -> Vec<Value> {
    sinks.borrow_mut().retain(|_, (global, _)| global.upgrade().is_some());
    let notifications = std::mem::take(&mut *pending.borrow_mut());
    let mut groups: Vec<(usize, BrowserRejectionSink, Vec<BrowserRejectionEvent>)> = Vec::new();
    for event in notifications {
        let sink = browser_realm_key_ctx(ctx, event.owner())
            .and_then(|key| sinks.borrow().get(&key).map(|(_, sink)| (key, sink.clone())));
        if let Some((key, sink)) = sink {
            if let Some((_, _, events)) = groups.iter_mut().find(|(previous, _, _)| *previous == key) {
                events.push(event);
            } else {
                groups.push((key, sink, vec![event]));
            }
        } else {
            pending.borrow_mut().push_back(event);
        }
    }
    let mut errors = Vec::new();
    for (_, sink, events) in groups {
        errors.extend(sink(ctx, events, delivery.clone()));
    }
    errors
}

struct BrowserRejectionRecord {
    realm_key: usize,
    promise: WeakValue,
    delivered: bool,
}

#[derive(Default)]
struct BrowserRejectionTracking {
    records: HashMap<usize, BrowserRejectionRecord>,
    cancelled_realms: Vec<(usize, WeakValue)>,
}

impl BrowserRejectionTracking {
    fn realm_is_live(&mut self, key: usize) -> bool {
        self.cancelled_realms.retain(|(_, weak)| weak.upgrade().is_some());
        !self.cancelled_realms.iter().any(|(cancelled, _)| *cancelled == key)
    }
}

/// Shared native notification state used by the runtime and actual browser UA tasks.
/// No tracker borrow may be held while dispatching an author event listener.
#[derive(Clone, Default)]
pub struct BrowserRejectionDelivery(Rc<RefCell<BrowserRejectionTracking>>);

impl BrowserRejectionDelivery {
    /// Check immediately before dispatch, after any earlier tasks or list entries ran.
    pub fn should_dispatch(&self, ctx: &mut Ctx, owner: &RealmHandle, unhandled: bool, promise: &Value) -> bool {
        let Some(key) = ctx.object_addr(&owner.global()) else {
            return false;
        };
        let mut state = self.0.borrow_mut();
        if !state.realm_is_live(key) {
            return false;
        }
        if !unhandled {
            return true;
        }
        let Some(identity) = ctx.object_addr(promise) else {
            return false;
        };
        let same = state.records.get(&identity).is_some_and(|record| {
            record.realm_key == key
                && !record.delivered
                && record.promise.upgrade().and_then(|tracked| ctx.object_addr(&tracked)) == Some(identity)
        });
        if !same {
            return false;
        }
        if ctx.promise_is_handled(promise) != Some(false) {
            state.records.remove(&identity);
            return false;
        }
        true
    }

    /// Release a pending notification whose event could not be dispatched.
    pub fn discard_pending(&self, ctx: &mut Ctx, promise: &Value) {
        if let Some(identity) = ctx.object_addr(promise) {
            let mut state = self.0.borrow_mut();
            if state.records.get(&identity).is_some_and(|record| !record.delivered) {
                state.records.remove(&identity);
            }
        }
    }

    /// Publish outstanding membership only after genuine unhandled dispatch completed.
    pub fn did_dispatch_unhandled(&self, ctx: &mut Ctx, owner: &RealmHandle, promise: &Value) {
        let Some(identity) = ctx.object_addr(promise) else {
            return;
        };
        let Some(key) = ctx.object_addr(&owner.global()) else {
            return;
        };
        let mut state = self.0.borrow_mut();
        if !state.realm_is_live(key) || ctx.promise_is_handled(promise) != Some(false) {
            state.records.remove(&identity);
            return;
        }
        if let Some(record) = state.records.get_mut(&identity) {
            if record.realm_key == key {
                record.delivered = true;
            }
        }
    }
}

/// One host task's worth of notifications: a whole checkpoint list, or a single handled event.
pub enum BrowserRejectionTask {
    Unhandled {
        batch: BrowserRejectionBatch,
        entries: Vec<(RealmHandle, Value, Value)>,
    },
    Handled {
        owner: RealmHandle,
        promise: Value,
        reason: Value,
    },
}

/// Split notifications into host tasks, preserving checkpoint-list boundaries and intervening
/// handled notifications.
pub fn group_rejection_tasks(notifications: Vec<BrowserRejectionEvent>) -> Vec<BrowserRejectionTask> {
    let mut tasks: Vec<BrowserRejectionTask> = Vec::new();
    for notification in notifications {
        match notification {
            BrowserRejectionEvent::Unhandled { batch, owner, promise, reason } => {
                if let Some(BrowserRejectionTask::Unhandled { batch: previous, entries }) = tasks.last_mut() {
                    if previous.same_batch(&batch) {
                        entries.push((owner, promise, reason));
                        continue;
                    }
                }
                tasks.push(BrowserRejectionTask::Unhandled { batch, entries: vec![(owner, promise, reason)] });
            }
            BrowserRejectionEvent::Handled { owner, promise, reason } => {
                tasks.push(BrowserRejectionTask::Handled { owner, promise, reason });
            }
        }
    }
    tasks
}

/// Run one host task. `dispatch(ctx, type, promise, reason)` returns `false` when default was
/// prevented. Each unhandled entry re-checks handling just before firing; the first dispatch
/// error is returned after every entry ran.
pub fn run_rejection_task<E>(
    ctx: &mut Ctx,
    delivery: &BrowserRejectionDelivery,
    task: BrowserRejectionTask,
    mut dispatch: impl FnMut(&mut Ctx, &str, Value, Value) -> Result<bool, E>,
) -> Result<(), E> {
    match task {
        BrowserRejectionTask::Handled { owner, promise, reason } => {
            if delivery.should_dispatch(ctx, &owner, false, &promise) {
                dispatch(ctx, "rejectionhandled", promise, reason)?;
            }
            Ok(())
        }
        BrowserRejectionTask::Unhandled { entries, .. } => {
            let mut first_error = None;
            for (event_owner, promise, reason) in entries {
                if !delivery.should_dispatch(ctx, &event_owner, true, &promise) {
                    continue;
                }
                match dispatch(ctx, "unhandledrejection", promise.clone(), reason) {
                    Ok(_) => delivery.did_dispatch_unhandled(ctx, &event_owner, &promise),
                    Err(error) => {
                        delivery.discard_pending(ctx, &promise);
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
            first_error.map_or(Ok(()), Err)
        }
    }
}

/// The collection, sink and delivery state for one runtime's browser rejection policy.
#[derive(Default)]
pub struct BrowserRejectionPolicy {
    enabled: bool,
    pending: PendingRejections,
    sinks: BrowserRejectionSinks,
    delivery: BrowserRejectionDelivery,
    errors: Rc<RefCell<BrowserAdmissionErrors>>,
}

impl BrowserRejectionPolicy {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Start collecting browser notifications and install the exact handler-attachment hook and
    /// the genuine HTML checkpoint hook. Idempotent.
    pub fn enable(&mut self, engine: &mut lumen::Engine) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        engine.track_late_handled_rejections();
        let hook_delivery = self.delivery.clone();
        let hook_pending = self.pending.clone();
        let hook_sinks = self.sinks.clone();
        let hook_errors = self.errors.clone();
        engine.ctx().set_native_rejection_handled_hook(Some(Rc::new(move |ctx, promise| {
            let Some(event) = collect_browser_handled(ctx, promise, &hook_delivery) else {
                return false;
            };
            let sink = browser_realm_key_ctx(ctx, event.owner())
                .and_then(|key| hook_sinks.borrow().get(&key).map(|(_, sink)| sink.clone()));
            if let Some(sink) = sink {
                record_browser_admission_errors(&hook_errors, sink(ctx, vec![event], hook_delivery.clone()));
            } else {
                hook_pending.borrow_mut().push_back(event);
            }
            true
        })));
        let delivery = self.delivery.clone();
        let pending = self.pending.clone();
        let sinks = self.sinks.clone();
        let errors = self.errors.clone();
        crate::scheduling::set_checkpoint_hook(engine, move |engine| {
            collect_browser_rejections(engine, &delivery, &pending);
            let mut failures = drain_browser_admission_errors(engine.ctx(), &errors);
            failures.extend(admit_browser_rejections(engine.ctx(), &delivery, &pending, &sinks));
            failures
        });
    }

    /// Collect, admit through registered sinks and return admission failures for the host to
    /// report. Used by loops that have their own checkpoint instead of the HTML one.
    pub fn checkpoint(&self, engine: &mut lumen::Engine) -> Vec<Value> {
        collect_browser_rejections(engine, &self.delivery, &self.pending);
        let mut errors = drain_browser_admission_errors(engine.ctx(), &self.errors);
        errors.extend(admit_browser_rejections(engine.ctx(), &self.delivery, &self.pending, &self.sinks));
        errors
    }

    /// Register native task admission for one live realm. The sink must retain its owner weakly.
    pub fn set_sink(&mut self, engine: &mut lumen::Engine, owner: &RealmHandle, sink: BrowserRejectionSink) {
        self.enable(engine);
        if let (Some(key), Some(global)) = (browser_realm_key(engine, owner), engine.ctx().weak_value(&owner.global())) {
            self.sinks.borrow_mut().insert(key, (global, sink));
        }
    }

    pub fn delivery(&self) -> BrowserRejectionDelivery {
        self.delivery.clone()
    }

    pub fn take_events(&self) -> Vec<BrowserRejectionEvent> {
        std::mem::take(&mut *self.pending.borrow_mut()).into_iter().collect()
    }

    /// Take only notifications belonging to one live realm, leaving other realms' events queued.
    pub fn take_events_for_realm(&self, engine: &mut lumen::Engine, realm: &RealmHandle) -> Vec<BrowserRejectionEvent> {
        let Some(key) = browser_realm_key(engine, realm) else {
            return Vec::new();
        };
        if !self.delivery.0.borrow_mut().realm_is_live(key) || self.pending.borrow().is_empty() {
            return Vec::new();
        }
        let mut selected = Vec::new();
        let queued = self.pending.borrow().len();
        for _ in 0..queued {
            let Some(event) = self.pending.borrow_mut().pop_front() else {
                break;
            };
            if event.owner().same_realm(realm) {
                selected.push(event);
            } else {
                self.pending.borrow_mut().push_back(event);
            }
        }
        selected
    }

    /// Cancel pending notifications and tracking for a realm being retired. A weak global marker
    /// also suppresses promises that reject after this call.
    pub fn cancel_for_realm(&self, engine: &mut lumen::Engine, realm: &RealmHandle) -> usize {
        let Some(key) = browser_realm_key(engine, realm) else {
            return 0;
        };
        let removed_sink = self.sinks.borrow_mut().remove(&key);
        let weak = engine.ctx().weak_value(&realm.global());
        let mut tracking = self.delivery.0.borrow_mut();
        tracking.cancelled_realms.retain(|(_, existing)| existing.upgrade().is_some());
        if !tracking.cancelled_realms.iter().any(|(existing, _)| *existing == key) {
            if let Some(weak) = weak {
                tracking.cancelled_realms.push((key, weak));
            }
        }
        let before_events = self.pending.borrow().len();
        self.pending.borrow_mut().retain(|event| !event.owner().same_realm(realm));
        let before_reasons = tracking.records.len();
        tracking.records.retain(|_, record| record.realm_key != key);
        let removed_records = before_reasons - tracking.records.len();
        drop(tracking);
        drop(removed_sink);
        let discarded = engine.discard_rejections_for_realm(realm);
        before_events - self.pending.borrow().len() + removed_records + discarded
    }

    /// Teardown: detach the handler hook and release every retained payload outside the
    /// `RefCell` borrows, since native owners captured by sinks can run teardown while dropping.
    pub fn release(&mut self, engine: &mut lumen::Engine) {
        engine.ctx().set_native_rejection_handled_hook(None);
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        let sinks = std::mem::take(&mut *self.sinks.borrow_mut());
        let tracking = std::mem::take(&mut *self.delivery.0.borrow_mut());
        let errors = std::mem::take(&mut *self.errors.borrow_mut());
        drop(pending);
        drop(sinks);
        drop(tracking);
        drop(errors);
    }
}

#[derive(Default)]
struct BrowserSinkDocuments(HashMap<usize, std::rc::Weak<DomRealm>>);

/// Register a Window document as the admission target for its creating realm (a child realm for
/// frames, the current root realm otherwise). Idempotent per document.
pub fn register_document_rejection_sink(
    policy: &mut BrowserRejectionPolicy,
    engine: &mut lumen::Engine,
    realm: &Rc<DomRealm>,
) -> Result<(), String> {
    let key = Rc::as_ptr(realm) as usize;
    let present = engine
        .ctx()
        .op_state()
        .get::<BrowserSinkDocuments>()
        .and_then(|documents| documents.0.get(&key))
        .and_then(std::rc::Weak::upgrade)
        .is_some_and(|previous| Rc::ptr_eq(&previous, realm));
    if present {
        return Ok(());
    }
    let owner = realm
        .child_realm_handle()
        .map_err(|error| format!("resolve rejection sink realm: {error:?}"))?
        .unwrap_or_else(|| engine.ctx().current_host_realm());
    let document = Rc::downgrade(realm);
    policy.set_sink(
        engine,
        &owner,
        Rc::new(move |ctx, notifications, delivery| {
            let Some(realm) = document.upgrade() else {
                return Vec::new();
            };
            match admit_document_rejection_events(ctx, &realm, notifications, delivery) {
                Ok(()) => Vec::new(),
                Err(error) => vec![ctx.make_error("Error", error)],
            }
        }),
    );
    let mut documents = engine
        .ctx()
        .op_state()
        .get::<BrowserSinkDocuments>()
        .map(|documents| documents.0.clone())
        .unwrap_or_default();
    documents.retain(|_, document| document.strong_count() != 0);
    documents.insert(key, Rc::downgrade(realm));
    engine.ctx().op_state().put(BrowserSinkDocuments(documents));
    Ok(())
}

/// Admit the document realm's pending notifications as real UA tasks now.
pub fn queue_document_rejection_events(
    policy: &mut BrowserRejectionPolicy,
    engine: &mut lumen::Engine,
    realm: &Rc<DomRealm>,
) -> Result<(), String> {
    register_document_rejection_sink(policy, engine, realm)?;
    let owner = realm
        .child_realm_handle()
        .map_err(|error| format!("resolve rejection-event realm: {error:?}"))?
        .unwrap_or_else(|| engine.ctx().current_host_realm());
    let notifications = policy.take_events_for_realm(engine, &owner);
    admit_document_rejection_events(engine.ctx(), realm, notifications, policy.delivery())
}

/// Queue each notification task on the Window's user-agent task queue, in the creating realm.
pub fn admit_document_rejection_events(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    notifications: Vec<BrowserRejectionEvent>,
    delivery: BrowserRejectionDelivery,
) -> Result<(), String> {
    let Some(first) = notifications.first() else {
        return Ok(());
    };
    let owner = first.owner().clone();
    for task in group_rejection_tasks(notifications) {
        let realm = Rc::clone(realm);
        let delivery = delivery.clone();
        ctx.with_host_realm(&owner, move |ctx| {
            crate::scheduling::queue_task(ctx, move |ctx| {
                run_rejection_task(ctx, &delivery, task, |ctx, kind, promise, reason| {
                    realm.dispatch_promise_rejection(ctx, kind, promise, reason)
                })
            })
        })
        .map_err(|error| format!("enter Window rejection-event realm: {error}"))?
        .map_err(|error| format!("queue Window rejection event task: {error:?}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_rejection_admission_errors_preserve_first_identity_and_bound_additional_causes() {
        let mut engine = lumen::Engine::new();
        let state = Rc::new(RefCell::new(BrowserAdmissionErrors::default()));
        let first = lumen::embed::OpError::new("RangeError", "task quota exhausted").to_value(engine.ctx());
        let identity = engine.ctx().object_addr(&first);
        record_browser_admission_errors(&state, vec![first]);
        for _ in 0..100 {
            let error = lumen::embed::OpError::new("Error", "additional task admission failed").to_value(engine.ctx());
            record_browser_admission_errors(&state, vec![error]);
        }
        assert_eq!(state.borrow().count, 101);
        let errors = drain_browser_admission_errors(engine.ctx(), &state);
        assert_eq!(errors.len(), 2);
        assert_eq!(engine.ctx().object_addr(&errors[0]), identity);
        let message = engine.ctx().get_member(&errors[1], "message").unwrap_or(Value::Undefined);
        assert!(
            matches!(message, Value::Str(ref value) if value.as_str() == "100 additional browser rejection task admission failures")
        );
        assert!(drain_browser_admission_errors(engine.ctx(), &state).is_empty());
        state.borrow_mut().count = u64::MAX;
        let first = lumen::embed::OpError::new("Error", "overflow admission").to_value(engine.ctx());
        record_browser_admission_errors(&state, vec![first]);
        assert!(state.borrow().overflow);
        assert_eq!(drain_browser_admission_errors(engine.ctx(), &state).len(), 2);
    }
}
