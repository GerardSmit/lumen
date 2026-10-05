//! Host-driven rendering callbacks. A rendering opportunity is supplied by the
//! embedder; requesting a callback never manufactures a timer or a frame.
use super::*;
use lumen::embed::{JsFunction, JsObject};
use lumen_host::time::Instant;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

#[derive(Clone)]
struct CheckpointHook(Rc<dyn Fn(&mut lumen::Engine) -> Vec<Value>>);

/// Attach the embedder's native notification phase to genuine HTML callback
/// checkpoints. The callback must not retain the engine or invoke author hooks.
pub fn set_checkpoint_hook(
    engine: &mut lumen::Engine,
    hook: impl Fn(&mut lumen::Engine) -> Vec<Value> + 'static,
) {
    engine.ctx().op_state().put(CheckpointHook(Rc::new(hook)));
}

fn checkpoint(engine: &mut lumen::Engine) -> Vec<Value> {
    while engine.run_one_job() {}
    let hook = engine.ctx().op_state().get::<CheckpointHook>().cloned();
    // Drain native producer diagnostics independently of the embedder's Promise
    // notification hook. Error materialization happens after releasing the queue.
    let mut errors = drain_task_diagnostics(engine.ctx());
    errors.extend(hook.map_or_else(Vec::new, |hook| (hook.0)(engine)));
    errors
}

struct HtmlTask {
    id: u64,
    realm: lumen::embed::RealmHandle,
    callback: Option<HtmlTaskCallback>,
    ready: Option<Rc<Cell<bool>>>,
    eligible_this_turn: bool,
}

type HtmlTaskCallback = Box<dyn FnOnce(&mut Ctx) -> OpResult<()>>;

/// Bound retained callback closures across the host's live realms. Producers
/// share this queue rather than allocating independent unbounded task lists.
pub(crate) const MAX_PENDING_HTML_TASKS: usize = 4096;

#[derive(Default)]
struct TaskQueueState {
    next_id: u64,
    tasks: VecDeque<HtmlTask>,
    gated: usize,
    owners: Vec<Rc<TaskOwner>>,
    diagnostics: TaskDiagnostics,
    idle_scan_epoch: Cell<Option<u64>>,
}

thread_local! {
    static READINESS_EPOCH: Cell<u64> = const { Cell::new(0) };
}

/// Flip a task readiness cell and signal queues that a not-ready scan is stale.
pub(crate) fn mark_task_ready(ready: &Cell<bool>) {
    if !ready.replace(true) {
        READINESS_EPOCH.with(|epoch| epoch.set(epoch.get().wrapping_add(1)));
    }
}

fn readiness_epoch() -> u64 { READINESS_EPOCH.with(Cell::get) }

impl TaskQueueState {
    /// True when every queued task is gated and no readiness cell flipped since
    /// a scan already found them all not-ready.
    fn scan_known_idle(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.len() == self.gated
            && self.idle_scan_epoch.get() == Some(readiness_epoch())
    }
    fn release_idle_capacity(&mut self) {
        if self.tasks.is_empty() && self.tasks.capacity() > 64 {
            self.tasks = VecDeque::new();
        } else if self.tasks.capacity() > 256 && self.tasks.len() * 4 < self.tasks.capacity() {
            self.tasks.shrink_to(self.tasks.len().max(64) * 2);
        }
    }
}

/// Fixed native contexts: diagnostics never retain author values, callbacks,
/// nodes, realm handles or leases while a Document mutation borrow is active.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TaskDiagnosticSource { DetailsToggle, DialogToggle, PopoverToggle }

#[derive(Clone, Copy, Debug)]
pub(crate) enum TaskDiagnosticCause {
    QueueFull, AllocationFailed, SequenceExhausted, OwnerRetired,
    TrackerAllocationFailed, TrackerSequenceExhausted, QueueDisposed, ProducerAllocationFailed,
}

const DIAGNOSTIC_SOURCES: [TaskDiagnosticSource; 3] = [
    TaskDiagnosticSource::DetailsToggle, TaskDiagnosticSource::DialogToggle,
    TaskDiagnosticSource::PopoverToggle,
];
const DIAGNOSTIC_CAUSES: [TaskDiagnosticCause; 8] = [
    TaskDiagnosticCause::QueueFull, TaskDiagnosticCause::AllocationFailed,
    TaskDiagnosticCause::SequenceExhausted, TaskDiagnosticCause::OwnerRetired,
    TaskDiagnosticCause::TrackerAllocationFailed, TaskDiagnosticCause::TrackerSequenceExhausted,
    TaskDiagnosticCause::QueueDisposed, TaskDiagnosticCause::ProducerAllocationFailed,
];

#[derive(Default)]
struct TaskDiagnostics { counts: [[u64; 8]; 3], overflow: [[bool; 8]; 3] }

impl TaskDiagnostics {
    fn record(&mut self, source: TaskDiagnosticSource, cause: TaskDiagnosticCause) {
        let (source, cause) = (source as usize, cause as usize);
        match self.counts[source][cause].checked_add(1) {
            Some(count) => self.counts[source][cause] = count,
            None => self.overflow[source][cause] = true,
        }
    }
    fn pending(&self) -> bool { self.counts.iter().flatten().any(|count| *count != 0) }
}

fn drain_task_diagnostics(ctx: &mut Ctx) -> Vec<Value> {
    let queue = ctx.op_state().get::<TaskQueue>().cloned();
    let Some(queue) = queue else { return Vec::new(); };
    let diagnostics = {
        let mut queue = queue.0.borrow_mut();
        if !queue.diagnostics.pending() { return Vec::new(); }
        std::mem::take(&mut queue.diagnostics)
    };
    let mut message = String::from("HTML task producer failures:");
    for source in DIAGNOSTIC_SOURCES {
        for cause in DIAGNOSTIC_CAUSES {
            let count = diagnostics.counts[source as usize][cause as usize];
            if count != 0 {
                use std::fmt::Write;
                let _ = write!(message, " {:?}/{:?}: count={}{};", source, cause, count,
                    if diagnostics.overflow[source as usize][cause as usize] {
                        " (count overflow; additional occurrences not representable)"
                    } else { "" });
            }
        }
    }
    vec![ctx.make_error("Error", message)]
}

#[derive(Clone, Default)]
struct TaskQueue(Rc<RefCell<TaskQueueState>>);

struct TaskOwner {
    realm: lumen::embed::RealmHandle,
}

/// A document may capture task admission without retaining its Window/global.
/// The engine owns registrations; retirement invalidates all captured senders.
#[derive(Clone)]
pub(crate) struct TaskSender {
    queue: std::rc::Weak<RefCell<TaskQueueState>>,
    owner: std::rc::Weak<TaskOwner>,
}

/// The caller retains rejected work so leases can be released after leaving a
/// Document borrow. Admission never drops a rejected callback under that borrow.
pub(crate) struct TaskAdmissionFailure {
    pub error: OpError,
    pub cause: TaskDiagnosticCause,
    pub callback: HtmlTaskCallback,
}

fn task_queue(ctx: &mut Ctx) -> Rc<RefCell<TaskQueueState>> {
    if let Some(queue) = ctx.op_state().get::<TaskQueue>() {
        return queue.0.clone();
    }
    let queue = TaskQueue::default();
    let state = queue.0.clone();
    ctx.op_state().put(queue);
    state
}

pub(crate) fn task_sender(ctx: &mut Ctx) -> OpResult<TaskSender> {
    let queue = task_queue(ctx);
    let realm = ctx.current_host_realm();
    let owner = {
        let mut state = queue.borrow_mut();
        if let Some(owner) = state.owners.iter().find(|owner| owner.realm.same_realm(&realm)) {
            owner.clone()
        } else {
            if state.owners.len() >= MAX_PENDING_HTML_TASKS {
                return Err(OpError::new("QuotaExceededError", "HTML task owner registry is full"));
            }
            state.owners.try_reserve(1)
                .map_err(|_| OpError::new("QuotaExceededError", "HTML task owner allocation failed"))?;
            let owner = Rc::new(TaskOwner { realm });
            state.owners.push(owner.clone());
            owner
        }
    };
    Ok(TaskSender { queue: Rc::downgrade(&queue), owner: Rc::downgrade(&owner) })
}

fn admit_task(
    queue: &Rc<RefCell<TaskQueueState>>,
    realm: lumen::embed::RealmHandle,
    callback: HtmlTaskCallback,
    ready: Option<Rc<Cell<bool>>>,
) -> Result<u64, TaskAdmissionFailure> {
    let mut queue = queue.borrow_mut();
    let failure = if queue.tasks.len() >= MAX_PENDING_HTML_TASKS {
        Some((TaskDiagnosticCause::QueueFull, OpError::new("QuotaExceededError", "HTML task queue is full")))
    } else if queue.tasks.try_reserve(1).is_err() {
        Some((TaskDiagnosticCause::AllocationFailed, OpError::new("QuotaExceededError", "HTML task allocation failed")))
    } else if queue.next_id == u64::MAX {
        Some((TaskDiagnosticCause::SequenceExhausted, OpError::new("QuotaExceededError", "HTML task sequence exhausted")))
    } else { None };
    if let Some((cause, error)) = failure {
        return Err(TaskAdmissionFailure { error, cause, callback });
    }
    queue.next_id += 1;
    let id = queue.next_id;
    queue.idle_scan_epoch.set(None);
    if ready.is_some() { queue.gated += 1; }
    queue.tasks.push_back(HtmlTask { id, realm, callback: Some(callback), ready, eligible_this_turn: false });
    Ok(id)
}

pub(crate) struct TaskHandle {
    queue: std::rc::Weak<RefCell<TaskQueueState>>,
    id: u64,
}

impl TaskHandle {
    /// Adoption changes the actual task owner without cancelling/requeueing it
    /// or changing its position in the existing HTML task source.
    pub(crate) fn retarget(&self, sender: &TaskSender) -> OpResult<()> {
        let queue = self.queue.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "HTML task queue disposed"))?;
        let target_queue = sender.queue.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "HTML task queue disposed"))?;
        if !Rc::ptr_eq(&queue, &target_queue) {
            return Err(OpError::new("InvalidStateError", "HTML task cannot move between engines"));
        }
        let owner = sender.owner.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "HTML task owner retired"))?;
        let mut queue = queue.borrow_mut();
        let index = queue.tasks.partition_point(|task| task.id < self.id);
        if let Some(task) = queue.tasks.get_mut(index).filter(|task| task.id == self.id) {
            task.realm = owner.realm.clone();
        }
        Ok(())
    }
}

impl TaskSender {
    pub(crate) fn is_live(&self) -> bool {
        self.owner.upgrade().is_some() && self.queue.upgrade().is_some()
    }

    /// Callback-free, allocation-free admission reporting. Retired owners may
    /// still report to the live engine; retirement never discards diagnostics.
    /// False means the engine queue is disposed and no host remains to report to.
    pub(crate) fn record_failure(&self, source: TaskDiagnosticSource, cause: TaskDiagnosticCause) -> bool {
        let Some(queue) = self.queue.upgrade() else { return false; };
        queue.borrow_mut().diagnostics.record(source, cause);
        true
    }

    pub(crate) fn queue(
        &self, callback: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static,
    ) -> Result<(), TaskAdmissionFailure> {
        self.queue_with_readiness(None, Box::new(callback)).map(|_| ())
    }

    /// Prepared native documents admit work at mutation time, but cannot run
    /// author callbacks before their target arena is installed.
    pub(crate) fn queue_when_ready(
        &self, ready: Rc<Cell<bool>>,
        callback: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static,
    ) -> Result<(), TaskAdmissionFailure> {
        self.queue_with_readiness(Some(ready), Box::new(callback)).map(|_| ())
    }

    pub(crate) fn queue_tracked_when_ready(
        &self, ready: Rc<Cell<bool>>,
        callback: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static,
    ) -> Result<TaskHandle, TaskAdmissionFailure> {
        self.queue_with_readiness(Some(ready), Box::new(callback))
    }

    fn queue_with_readiness(&self, ready: Option<Rc<Cell<bool>>>, callback: HtmlTaskCallback) -> Result<TaskHandle, TaskAdmissionFailure> {
        let Some(queue) = self.queue.upgrade() else {
            return Err(TaskAdmissionFailure {
                error: OpError::new("InvalidStateError", "HTML task queue disposed"),
                cause: TaskDiagnosticCause::QueueDisposed, callback,
            });
        };
        let Some(owner) = self.owner.upgrade() else {
            return Err(TaskAdmissionFailure {
                error: OpError::new("InvalidStateError", "HTML task owner retired"),
                cause: TaskDiagnosticCause::OwnerRetired, callback,
            });
        };
        admit_task(&queue, owner.realm.clone(), callback, ready)
            .map(|id| TaskHandle { queue: Rc::downgrade(&queue), id })
    }
}

/// Release queued user-agent and rendering callbacks owned by a retired realm.
/// Other realms' work and separately retained author functions remain intact.
pub fn cancel_tasks_for_realm(ctx: &mut Ctx, realm: &lumen::embed::RealmHandle) -> usize {
    let mut count = 0;
    let mut callbacks = Vec::new();
    if let Some(queue) = ctx.op_state().get::<TaskQueue>().cloned() {
        let mut queue = queue.0.borrow_mut();
        queue.owners.retain(|owner| !owner.realm.same_realm(realm));
        let before = queue.tasks.len();
        let mut gated_removed = 0;
        queue.tasks.retain_mut(|task| {
            if task.realm.same_realm(realm) {
                if task.ready.is_some() { gated_removed += 1; }
                if let Some(callback) = task.callback.take() { callbacks.push(callback); }
                false
            } else { true }
        });
        queue.gated -= gated_removed;
        count += before - queue.tasks.len();
        queue.idle_scan_epoch.set(None);
        queue.release_idle_capacity();
        let owners_target = queue.owners.len().max(8) * 2;
        queue.owners.shrink_to(owners_target);
    }
    // Native lease destructors may admit other realms' work. Release the queue
    // borrow before dropping callbacks, and invalidate retired senders first.
    drop(callbacks);
    if let Some(state) =
        super::realm_services::RealmServices::<RefCell<Scheduler>>::remove_for_global(
            ctx,
            &realm.global(),
        )
    {
        let mut state = state.borrow_mut();
        count += state.frames.len() + state.idle.len();
        state.frames.clear();
        state.frame_order.clear();
        state.idle.clear();
        state.idle_order.clear();
    }
    count
}

/// Queue a user-agent task separately from Promise and mutation microtasks.
/// Captured JS values stay retained until invocation or realm teardown.
pub fn queue_task(
    ctx: &mut Ctx,
    task: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static,
) -> OpResult<()> {
    let realm = ctx.current_host_realm();
    admit_task(&task_queue(ctx), realm, Box::new(task), None).map(|_| ()).map_err(|failure| failure.error)
}

pub fn task_pending(ctx: &mut Ctx) -> bool {
    ctx.op_state().get::<TaskQueue>()
        .is_some_and(|queue| { let queue = queue.0.borrow();
            if queue.tasks.len() > queue.gated || queue.diagnostics.pending() { return true; }
            if queue.tasks.is_empty() || queue.scan_known_idle() { return false; }
            let any = queue.tasks.iter().any(task_ready);
            if !any { queue.idle_scan_epoch.set(Some(readiness_epoch())); }
            any })
}

fn task_ready(task: &HtmlTask) -> bool {
    task.ready.as_ref().is_none_or(|ready| ready.get())
}

/// Run admitted tasks with checkpoints between callbacks. New tasks wait for
/// a later host turn, and one callback's exception does not drop other tasks.
pub fn run_tasks(engine: &mut lumen::Engine, budget: usize) -> Vec<Value> {
    let mut errors = checkpoint(engine);
    let queue = engine.ctx().op_state().get::<TaskQueue>().cloned();
    let (count, boundary) = queue.as_ref().map_or((0, None), |queue| {
        let mut queue = queue.0.borrow_mut();
        if queue.scan_known_idle() { return (0, None); }
        let mut count = 0;
        let mut boundary = None;
        for task in queue.tasks.iter_mut() {
            if count >= budget { break; }
            task.eligible_this_turn = task_ready(task);
            if task.eligible_this_turn {
                count += 1;
                boundary = Some(task.id);
            }
        }
        if boundary.is_none() {
            queue.idle_scan_epoch.set(Some(readiness_epoch()));
            queue.release_idle_capacity();
        }
        (count, boundary)
    });
    let Some(boundary) = boundary else {
        return errors;
    };
    for _ in 0..count {
        let task = queue.as_ref().and_then(|queue| {
            let mut queue = queue.0.borrow_mut();
            let task = if queue.tasks.front().is_some_and(|task| task.id <= boundary && task.eligible_this_turn && task_ready(task)) {
                queue.tasks.pop_front()
            } else {
                let index = queue.tasks.iter().position(|task| task.id <= boundary && task.eligible_this_turn && task_ready(task));
                index.and_then(|index| queue.tasks.remove(index))
            };
            if task.as_ref().is_some_and(|task| task.ready.is_some()) { queue.gated -= 1; }
            task
        });
        if task.is_none() {
            break;
        }
        if let Some(mut task) = task {
            match engine.ctx().with_host_realm(&task.realm, |ctx| {
                (task.callback.take().expect("admitted HTML task callback"))(ctx)
                    .map_err(|error| error.to_value(ctx))
            }) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(engine.ctx().make_error("Error", error.to_string())),
            }
            errors.extend(checkpoint(engine));
        }
    }
    if let Some(queue) = queue.as_ref() {
        queue.0.borrow_mut().release_idle_capacity();
    }
    errors
}

struct IdleRequest {
    callback: JsFunction,
    timeout: Option<Instant>,
}

struct Scheduler {
    next: u32,
    frames: BTreeMap<u32, JsFunction>,
    frame_order: Vec<u32>,
    idle: BTreeMap<u32, IdleRequest>,
    idle_order: Vec<u32>,
}

fn scheduler(ctx: &mut Ctx) -> Option<Rc<RefCell<Scheduler>>> {
    super::realm_services::RealmServices::current(ctx)
}

impl Scheduler {
    fn allocate(&mut self) -> u32 {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next != 0
                && !self.frames.contains_key(&self.next)
                && !self.idle.contains_key(&self.next)
            {
                return self.next;
            }
        }
    }
}

#[lumen_bind::class(name = "IdleDeadline", hint(js(webidl)))]
struct IdleDeadline {
    timeout: bool,
    deadline: Instant,
}

#[lumen_bind::methods]
impl IdleDeadline {
    #[getter]
    fn did_timeout(&self) -> bool {
        self.timeout
    }

    fn time_remaining(&self) -> f64 {
        self.deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            * 1000.0
    }
}

#[lumen_bind::module(name = "rendering_callbacks")]
mod globals {
    use super::*;

    #[op(rename(js = "requestAnimationFrame"))]
    fn request_animation_frame(ctx: &mut Ctx, callback: JsFunction) -> u32 {
        let state = scheduler(ctx).expect("rendering callbacks installed");
        let mut state = state.borrow_mut();
        let id = state.allocate();
        state.frames.insert(id, callback);
        state.frame_order.push(id);
        id
    }

    #[op(rename(js = "cancelAnimationFrame"))]
    fn cancel_animation_frame(ctx: &mut Ctx, handle: Value) -> OpResult<()> {
        let handle = super::super::ui_events::number_to_unsigned(
            ctx.coerce_number(&handle).map_err(OpError::thrown)?,
            32,
        );
        let state = scheduler(ctx).expect("rendering callbacks installed");
        let mut state = state.borrow_mut();
        state.frames.remove(&handle);
        state.frame_order.retain(|id| *id != handle);
        Ok(())
    }

    #[op(rename(js = "requestIdleCallback"))]
    fn request_idle_callback(
        ctx: &mut Ctx,
        callback: JsFunction,
        options: Option<JsObject>,
    ) -> OpResult<u32> {
        let timeout = if let Some(options) = options {
            match options.get(ctx, "timeout")? {
                Value::Undefined => None,
                value => {
                    let millis = ctx.coerce_number(&value).map_err(OpError::thrown)?;
                    // WebIDL unsigned-long conversion, including wrapping.
                    let millis = super::super::ui_events::number_to_unsigned(millis, 32);
                    Some(Instant::now() + Duration::from_millis(u64::from(millis)))
                }
            }
        } else {
            None
        };
        let state = scheduler(ctx).expect("rendering callbacks installed");
        let mut state = state.borrow_mut();
        let id = state.allocate();
        state.idle.insert(id, IdleRequest { callback, timeout });
        state.idle_order.push(id);
        Ok(id)
    }

    #[op(rename(js = "cancelIdleCallback"))]
    fn cancel_idle_callback(ctx: &mut Ctx, handle: Value) -> OpResult<()> {
        let handle = super::super::ui_events::number_to_unsigned(
            ctx.coerce_number(&handle).map_err(OpError::thrown)?,
            32,
        );
        let state = scheduler(ctx).expect("rendering callbacks installed");
        let mut state = state.borrow_mut();
        state.idle.remove(&handle);
        state.idle_order.retain(|id| *id != handle);
        Ok(())
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    lumen_host::perf::start_clock();
    super::realm_services::RealmServices::replace_current(
        ctx,
        RefCell::new(Scheduler {
            next: 0,
            frames: BTreeMap::new(),
            frame_order: Vec::new(),
            idle: BTreeMap::new(),
            idle_order: Vec::new(),
        }),
    );
    let constructor = ctx.class_constructor::<IdleDeadline>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "IdleDeadline", constructor)
        .map_err(|_| ctx.make_error("Error", "IdleDeadline install failed"))?;
    ctx.install_module::<globals::Module>(&global)
}

/// Whether a rendering opportunity must wake this realm.
pub fn animation_frame_pending(ctx: &mut Ctx) -> bool {
    scheduler(ctx).is_some_and(|state| !state.borrow().frames.is_empty())
        || super::animations::pending(ctx)
}

/// Earliest idle timeout, or zero when an idle opportunity could do useful work.
pub fn idle_delay_ms(ctx: &mut Ctx) -> Option<u64> {
    if task_pending(ctx) {
        return Some(0);
    }
    let state = scheduler(ctx)?;
    if state.borrow().idle.is_empty() {
        None
    } else {
        Some(0)
    }
}

/// Timeout wakeups remain necessary while the host is busy rendering, whereas
/// ordinary idle work waits for a genuine idle opportunity.
pub fn idle_timeout_delay_ms(ctx: &mut Ctx) -> Option<u64> {
    if task_pending(ctx) {
        return Some(0);
    }
    let now = Instant::now();
    let state = scheduler(ctx)?;
    let state = state.borrow();
    state
        .idle
        .values()
        .filter_map(|request| request.timeout)
        .map(|timeout| {
            let delay = timeout.saturating_duration_since(now);
            delay.as_millis().min(u128::from(u64::MAX)) as u64
                + u64::from(delay.subsec_nanos() % 1_000_000 != 0)
        })
        .min()
}

/// HTML reports author callback exceptions in the callback's relevant realm,
/// which can differ from the document owning the rendering queue. Returned
/// errors remain available to embedders for diagnostics; they are already reported.
fn invoke_rendering_callback(
    ctx: &mut Ctx,
    callback: &JsFunction,
    this: Value,
    arguments: &[Value],
) -> Result<Value, Value> {
    let callback_realm = match ctx.function_host_realm(callback) {
        Ok(realm) => realm,
        Err(error) => {
            // A revoked proxy cannot supply a relevant realm. Report its lookup
            // failure in the invoking document without trying to execute it.
            super::DomRealm::report_exception(ctx, error.clone());
            return Err(error);
        }
    };
    callback.call(ctx, this, arguments).map_err(|error| {
        let error = error.to_value(ctx);
        let reported = ctx.with_host_realm(&callback_realm, |ctx| {
            super::DomRealm::report_exception(ctx, error.clone());
        });
        if reported.is_err() {
            // A realm unavailable to the embedder must not discard the original
            // throw or replace it with a host-scope diagnostic.
            super::DomRealm::report_exception(ctx, error.clone());
        }
        error
    })
}

/// Run one rendering opportunity. Snapshot handles, but remove each entry only
/// immediately before invocation: an earlier callback may cancel a later one.
/// Requests made by callbacks are left for the next opportunity. Returned author
/// callback errors have already been reported to their Window.
pub fn run_animation_frame(engine: &mut lumen::Engine) -> Vec<Value> {
    let realm = engine.ctx().current_host_realm();
    run_animation_frame_in_realm(engine, &realm)
}

/// Supply a rendering opportunity to one document without replacing the host's
/// active realm or draining another document's rendering requests.
pub fn run_animation_frame_in_realm(
    engine: &mut lumen::Engine,
    realm: &lumen::embed::RealmHandle,
) -> Vec<Value> {
    let timestamp = lumen_host::perf::web_now_ms();
    let mut errors = Vec::new();
    match engine.ctx().with_host_realm(realm, |ctx| {
        super::animations::advance(ctx, timestamp).map_err(|error| {
            let error = error.to_value(ctx);
            super::DomRealm::report_exception(ctx, error.clone());
            error
        })
    }) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(error),
        Err(error) => errors.push(engine.ctx().make_error("Error", error.to_string())),
    }
    errors.extend(checkpoint(engine));
    let Some(state) = engine
        .ctx()
        .with_host_realm(realm, scheduler)
        .ok()
        .flatten()
    else {
        return errors;
    };
    let handles = std::mem::take(&mut state.borrow_mut().frame_order);
    for handle in handles {
        let callback = {
            let mut state = state.borrow_mut();
            state.frames.remove(&handle)
        };
        if let Some(callback) = callback {
            match engine.ctx().with_host_realm(realm, |ctx| {
                let this = ctx.global_this();
                invoke_rendering_callback(ctx, &callback, this, &[Value::Num(timestamp)])
            }) {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(engine.ctx().make_error("Error", error.to_string())),
            }
            // The HTML callback invocation performs a microtask checkpoint.
            errors.extend(checkpoint(engine));
        }
    }
    errors
}

/// Run idle work within a host supplied budget (at most 50ms). Expired timeout
/// requests run even with no remaining idle time. Reentrant requests wait for
/// the next idle opportunity. Returned author callback errors are already reported.
pub fn run_idle_callbacks(engine: &mut lumen::Engine, budget_ms: u32) -> Vec<Value> {
    let realm = engine.ctx().current_host_realm();
    run_idle_callbacks_in_realm(engine, &realm, budget_ms)
}

/// Supply an idle opportunity to one document, preserving other realm queues.
pub fn run_idle_callbacks_in_realm(
    engine: &mut lumen::Engine,
    realm: &lumen::embed::RealmHandle,
    budget_ms: u32,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(budget_ms.min(50)));
    let Some(state) = engine
        .ctx()
        .with_host_realm(realm, scheduler)
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    let mut handles = std::mem::take(&mut state.borrow_mut().idle_order);
    let mut retained = 0;
    let mut errors = Vec::new();
    for index in 0..handles.len() {
        let handle = handles[index];
        let now = Instant::now();
        let mut state = state.borrow_mut();
        let Some(request) = state.idle.get(&handle) else {
            continue;
        };
        let timeout = request.timeout.is_some_and(|time| time <= now);
        if !timeout && now >= deadline {
            handles[retained] = handle;
            retained += 1;
            continue;
        }
        let request = state.idle.remove(&handle).unwrap();
        drop(state);
        match engine.ctx().with_host_realm(realm, |ctx| {
            let argument = ctx.new_instance(IdleDeadline {
                timeout,
                deadline: if timeout { now } else { deadline },
            });
            let this = ctx.global_this();
            invoke_rendering_callback(ctx, &request.callback, this, &[argument])
        }) {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => errors.push(error),
            Err(error) => errors.push(engine.ctx().make_error("Error", error.to_string())),
        }
        errors.extend(checkpoint(engine));
    }
    // Keep older budget-deferred requests ahead of reentrant admissions. Filter
    // after callbacks so cancellation and realm retirement cannot retain roots.
    handles.truncate(retained);
    let mut state = state.borrow_mut();
    handles.retain(|handle| state.idle.contains_key(handle));
    handles.append(&mut state.idle_order);
    state.idle_order = handles;
    errors
}

#[cfg(test)]
mod tests {
    #[test]
    fn prepared_task_readiness_preserves_initial_turn_order() {
        let mut engine = lumen::Engine::new();
        let sender = task_sender(engine.ctx()).unwrap();
        let ready = Rc::new(Cell::new(false));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let observed = calls.clone();
        assert!(sender.queue_when_ready(ready.clone(), move |_| {
            observed.borrow_mut().push(1); Ok(())
        }).is_ok());
        assert!(!task_pending(engine.ctx()));
        assert!(run_tasks(&mut engine, 16).is_empty());
        let observed = calls.clone();
        let arm = ready.clone();
        queue_task(engine.ctx(), move |_| {
            observed.borrow_mut().push(2); arm.set(true); Ok(())
        }).unwrap();
        let observed = calls.clone();
        queue_task(engine.ctx(), move |_| {
            observed.borrow_mut().push(3); Ok(())
        }).unwrap();
        assert!(run_tasks(&mut engine, 16).is_empty());
        assert_eq!(*calls.borrow(), vec![2, 3]);
        assert!(task_pending(engine.ctx()));
        assert!(run_tasks(&mut engine, 16).is_empty());
        assert_eq!(*calls.borrow(), vec![2, 3, 1]);
        assert!(!task_pending(engine.ctx()));
    }

    use super::*;

    #[test]
    fn producer_diagnostics_wake_without_tasks_preserve_causes_and_checkpoint_hook() {
        let mut engine = lumen::Engine::new();
        let sender = task_sender(engine.ctx()).unwrap();
        let queue = engine.ctx().op_state().get::<TaskQueue>().unwrap().0.clone();
        // Exercise an actual exhausted queue sequence, not a fabricated error.
        queue.borrow_mut().next_id = u64::MAX;
        let failure = sender.queue(|_| Ok(())).err().unwrap();
        assert!(matches!(failure.cause, TaskDiagnosticCause::SequenceExhausted));
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, failure.cause));
        drop(failure.callback);
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, TaskDiagnosticCause::TrackerAllocationFailed));
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, TaskDiagnosticCause::TrackerAllocationFailed));
        assert!(task_pending(engine.ctx()));
        assert_eq!(idle_delay_ms(engine.ctx()), Some(0));
        let hooks = Rc::new(RefCell::new(0));
        let called = hooks.clone();
        set_checkpoint_hook(&mut engine, move |_| { *called.borrow_mut() += 1; Vec::new() });
        let errors = run_tasks(&mut engine, 0);
        assert_eq!(errors.len(), 1);
        let message = engine.ctx().get_member(&errors[0], "message").ok().expect("native error message");
        let message = engine.ctx().to_string(&message).ok().expect("native error message string");
        assert!(message.contains("DetailsToggle/SequenceExhausted: count=1"));
        assert!(message.contains("DetailsToggle/TrackerAllocationFailed: count=2"));
        assert_eq!(*hooks.borrow(), 1);
        assert!(!task_pending(engine.ctx()));
        assert!(run_tasks(&mut engine, 0).is_empty());
    }

    #[test]
    fn producer_queue_quota_diagnostic_survives_canceled_callbacks() {
        let mut engine = lumen::Engine::new();
        let child = engine.ctx().create_host_realm();
        let sender = engine.ctx().with_host_realm(&child, task_sender).unwrap().unwrap();
        for _ in 0..MAX_PENDING_HTML_TASKS { assert!(sender.queue(|_| Ok(())).is_ok()); }
        let failure = sender.queue(|_| Ok(())).err().unwrap();
        assert!(matches!(failure.cause, TaskDiagnosticCause::QueueFull));
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, failure.cause));
        drop(failure.callback);
        assert_eq!(cancel_tasks_for_realm(engine.ctx(), &child), MAX_PENDING_HTML_TASKS);
        // No retained task or realm is needed to keep the concrete failure live.
        assert!(task_pending(engine.ctx()));
        let ran = Rc::new(RefCell::new(false));
        let observed = ran.clone();
        queue_task(engine.ctx(), move |_| { *observed.borrow_mut() = true; Ok(()) }).unwrap();
        let errors = run_tasks(&mut engine, 1);
        assert_eq!(errors.len(), 1);
        assert!(*ran.borrow());
        let message = engine.ctx().get_member(&errors[0], "message").ok().expect("native error message");
        assert!(engine.ctx().to_string(&message).ok().expect("native error message string").contains("DetailsToggle/QueueFull: count=1"));
        assert!(!task_pending(engine.ctx()));
    }

    #[test]
    fn producer_diagnostics_survive_retirement_and_explicit_count_overflow() {
        let mut engine = lumen::Engine::new();
        let child = engine.ctx().create_host_realm();
        let sender = engine.ctx().with_host_realm(&child, task_sender).unwrap().unwrap();
        assert!(sender.is_live());
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, TaskDiagnosticCause::QueueFull));
        let queue = engine.ctx().op_state().get::<TaskQueue>().unwrap().0.clone();
        queue.borrow_mut().diagnostics.counts[TaskDiagnosticSource::DetailsToggle as usize]
            [TaskDiagnosticCause::QueueFull as usize] = u64::MAX;
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, TaskDiagnosticCause::QueueFull));
        cancel_tasks_for_realm(engine.ctx(), &child);
        assert!(!sender.is_live());
        let failure = sender.queue(|_| Ok(())).err().unwrap();
        assert!(matches!(failure.cause, TaskDiagnosticCause::OwnerRetired));
        assert!(sender.record_failure(TaskDiagnosticSource::DetailsToggle, failure.cause));
        drop(failure.callback);
        let errors = run_tasks(&mut engine, 0);
        assert_eq!(errors.len(), 1);
        let message = engine.ctx().get_member(&errors[0], "message").ok().expect("native error message");
        let message = engine.ctx().to_string(&message).ok().expect("native error message string");
        assert!(message.contains("count=18446744073709551615 (count overflow; additional occurrences not representable)"));
        assert!(message.contains("DetailsToggle/OwnerRetired: count=1"));
        assert!(!task_pending(engine.ctx()));
        drop(queue);
        drop(engine);
        assert!(!sender.is_live());
        assert!(!sender.record_failure(TaskDiagnosticSource::DetailsToggle, TaskDiagnosticCause::QueueDisposed));
        assert!(matches!(sender.queue(|_| Ok(())).err().unwrap().cause, TaskDiagnosticCause::QueueDisposed));
    }

    #[test]
    fn html_task_queue_bounds_admission_and_releases_retired_callbacks() {
        let mut engine = lumen::Engine::new();
        let mut ctx = engine.ctx();
        let realm = ctx.current_host_realm();
        let retained = Rc::new(());
        for _ in 0..MAX_PENDING_HTML_TASKS {
            let retained = retained.clone();
            queue_task(&mut ctx, move |_| {
                drop(retained);
                Ok(())
            }).unwrap();
        }
        assert_eq!(Rc::strong_count(&retained), MAX_PENDING_HTML_TASKS + 1);
        let rejected = retained.clone();
        assert!(queue_task(&mut ctx, move |_| {
            drop(rejected);
            Ok(())
        }).is_err());
        assert_eq!(Rc::strong_count(&retained), MAX_PENDING_HTML_TASKS + 1);
        {
            let queue = ctx.op_state().get::<TaskQueue>().unwrap().0.borrow();
            assert_eq!(queue.next_id, MAX_PENDING_HTML_TASKS as u64);
            assert_eq!(queue.tasks.len(), MAX_PENDING_HTML_TASKS);
        }
        assert_eq!(cancel_tasks_for_realm(&mut ctx, &realm), MAX_PENDING_HTML_TASKS);
        assert_eq!(Rc::strong_count(&retained), 1);
        queue_task(&mut ctx, |_| Ok(())).unwrap();
        assert_eq!(cancel_tasks_for_realm(&mut ctx, &realm), 1);
    }

    fn eval(engine: &mut lumen::Engine, source: &str) -> Value {
        match engine.eval_value(source) {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => panic!("scheduling test script threw"),
            Err(error) => panic!("scheduling test script parse failed: {}", error.message),
        }
    }

    #[test]
    fn captured_task_sender_shares_order_and_preserves_failed_callback_until_cleanup() {
        let mut engine = lumen::Engine::new();
        let sender = task_sender(engine.ctx()).unwrap();
        let order = Rc::new(RefCell::new(Vec::new()));
        let first = order.clone();
        sender.queue(move |_| { first.borrow_mut().push(1); Ok(()) }).ok().unwrap();
        let second = order.clone();
        queue_task(engine.ctx(), move |_| { second.borrow_mut().push(2); Ok(()) }).unwrap();
        assert!(run_tasks(&mut engine, 8).is_empty());
        assert_eq!(*order.borrow(), vec![1, 2]);
        for _ in 0..MAX_PENDING_HTML_TASKS {
            queue_task(engine.ctx(), |_| Ok(())).unwrap();
        }
        struct Release(Rc<RefCell<usize>>);
        impl Drop for Release {
            fn drop(&mut self) { *self.0.borrow_mut() += 1; }
        }
        let released = Rc::new(RefCell::new(0));
        let lease = Release(released.clone());
        let borrow = released.borrow_mut();
        let failure = sender.queue(move |_| { drop(lease); Ok(()) }).err().unwrap();
        assert_eq!(failure.error.class(), "QuotaExceededError");
        assert_eq!(*borrow, 0);
        drop(borrow);
        drop(failure.callback);
        assert_eq!(*released.borrow(), 1);
    }

    #[test]
    fn captured_task_sender_retirement_invalidates_clones_and_reentrant_cleanup_is_safe() {
        let mut engine = lumen::Engine::new();
        let parent = task_sender(engine.ctx()).unwrap();
        let child = engine.ctx().create_host_realm();
        let child_sender = engine.ctx().with_host_realm(&child, task_sender).unwrap().unwrap();
        let retained = child_sender.clone();
        struct QueueOnDrop(TaskSender, Rc<RefCell<usize>>);
        impl Drop for QueueOnDrop {
            fn drop(&mut self) {
                let completed = self.1.clone();
                self.0.queue(move |_| { *completed.borrow_mut() += 1; Ok(()) }).ok().unwrap();
            }
        }
        let completed = Rc::new(RefCell::new(0));
        let lease = QueueOnDrop(parent.clone(), completed.clone());
        child_sender.queue(move |_| { drop(lease); Ok(()) }).ok().unwrap();
        assert_eq!(cancel_tasks_for_realm(engine.ctx(), &child), 1);
        assert_eq!(retained.queue(|_| Ok(())).err().unwrap().error.class(), "InvalidStateError");
        assert!(run_tasks(&mut engine, 8).is_empty());
        assert_eq!(*completed.borrow(), 1);
        let queue = engine.ctx().op_state().get::<TaskQueue>().unwrap().0.clone();
        assert_eq!(queue.borrow().owners.len(), 1);
        drop(queue);
        drop(engine);
        assert!(parent.queue(|_| Ok(())).is_err());
    }

    #[test]
    fn rendering_exceptions_use_callback_realm_for_bound_and_proxy_functions() {
        let mut engine = lumen::Engine::new();
        let _parent_dom = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let _child_dom = engine.ctx().with_host_realm(&child, |ctx| {
            crate::install(ctx, "<main></main>", 64)
        }).unwrap().ok().expect("callback test script threw");
        engine.eval_value_in_host_realm(&child, r#"
            globalThis.callbackErrors = [];
            addEventListener('error', event => {
                callbackErrors.push(event.error instanceof Error);
                event.preventDefault();
            });
            function fail() { throw new Error('callback'); }
            globalThis.boundCallback = fail.bind(null);
            globalThis.proxyCallback = new Proxy(fail, {});
            const revocable = Proxy.revocable(fail, {});
            globalThis.revokedCallback = revocable.proxy;
            revocable.revoke();
        "#, false).unwrap().ok().expect("callback test script threw");
        for name in ["boundCallback", "proxyCallback", "revokedCallback"] {
            let function = engine.ctx().get_member(&child.global(), name).ok().expect("callback property unavailable");
            assert!(engine.ctx().member_set(&parent.global(), name, function).is_ok());
        }
        eval(&mut engine, r#"
            var parentErrors = [];
            addEventListener('error', event => {
                parentErrors.push(event.error instanceof TypeError);
                event.preventDefault();
            });
            requestAnimationFrame(boundCallback);
            requestAnimationFrame(proxyCallback);
            requestAnimationFrame(revokedCallback);
            requestIdleCallback(boundCallback);
        "#);
        assert_eq!(run_animation_frame(&mut engine).len(), 3);
        assert_eq!(run_idle_callbacks(&mut engine, 50).len(), 1);
        assert!(matches!(eval(&mut engine, "parentErrors.length===1&&parentErrors[0]"), Value::Bool(true)));
        let observed = engine.eval_value_in_host_realm(&child,
            "callbackErrors.length===3&&callbackErrors.every(Boolean)", false).unwrap().ok().expect("callback test script threw");
        assert!(matches!(observed, Value::Bool(true)));
    }

    #[test]
    fn animation_frame_and_document_timelines_share_one_stable_coarsened_sample() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        eval(&mut engine, r#"
            globalThis.timelineSamples = [];
            globalThis.customTimeline = new DocumentTimeline({originTime:0.002});
            globalThis.sampledAnimation = document.querySelector('main').animate([], 100000);
            requestAnimationFrame(timestamp => {
              timelineSamples.push(timestamp, document.timeline.currentTime,
                customTimeline.currentTime + 0.002);
              Promise.resolve().then(() => timelineSamples.push(document.timeline.currentTime));
            });
            requestAnimationFrame(timestamp => timelineSamples.push(timestamp));
        "#);
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(eval(&mut engine, r#"
            timelineSamples.length === 5 && timelineSamples.every(value =>
              Math.abs(value - timelineSamples[0]) < 1e-9) &&
              Math.abs(timelineSamples[0] * 10 - Math.round(timelineSamples[0] * 10)) < 1e-9
        "#), Value::Bool(true)));
        eval(&mut engine, "sampledAnimation.startTime = document.timeline.currentTime - 100;");
        // Wall-clock movement alone must not resample the document timeline.
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(matches!(eval(&mut engine,
            "document.timeline.currentTime === timelineSamples[0] && sampledAnimation.currentTime === 100"), Value::Bool(true)));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(eval(&mut engine,
            "document.timeline.currentTime > timelineSamples[0]"), Value::Bool(true)));
        let child = engine.ctx().create_host_realm();
        let _child_dom = engine.ctx().with_host_realm(&child, |ctx| {
            crate::install(ctx, "<main></main>", 64)
        }).unwrap().ok().expect("child document installed");
        engine.ctx().with_host_realm(&child, |ctx| {
            crate::animations::advance(ctx, 2000.1).unwrap();
        }).unwrap();
        let child_timeline = engine.eval_value_in_host_realm(&child,
            "document.timeline", false).unwrap().ok().expect("child timeline available");
        let global = engine.ctx().global_object();
        assert!(engine.ctx().member_set(&global, "foreignTimeline", child_timeline).is_ok());
        assert!(matches!(eval(&mut engine, r#"
            const readTimeline = Object.getOwnPropertyDescriptor(DocumentTimeline.prototype, 'currentTime').get;
            readTimeline.call(foreignTimeline) === 2000.1 &&
              document.timeline.currentTime !== 2000.1 &&
              document.implementation.createHTMLDocument('').timeline.currentTime === null
        "#), Value::Bool(true)));
    }

    #[test]
    fn rendering_queues_are_isolated_and_retirement_releases_only_owned_work() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        let parent = engine.ctx().current_host_realm();
        eval(&mut engine, "var parentFrames=0,parentIdles=0;requestAnimationFrame(()=>parentFrames++);requestIdleCallback(()=>parentIdles++,{timeout:0})");
        let child = engine.ctx().create_host_realm();
        assert!(engine
            .ctx()
            .with_host_realm(&child, install)
            .unwrap()
            .is_ok());
        let child_script = engine.eval_value_in_host_realm(&child, "var childFrames=0,childIdles=0,correctThis=false;requestAnimationFrame(function(){childFrames++;correctThis=this===globalThis});requestIdleCallback(()=>childIdles++,{timeout:0})", false).unwrap();
        assert!(child_script.is_ok());
        assert!(
            animation_frame_pending(engine.ctx()),
            "installing child providers must preserve parent callbacks"
        );
        assert!(run_animation_frame_in_realm(&mut engine, &child).is_empty());
        assert!(run_idle_callbacks_in_realm(&mut engine, &child, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "parentFrames===0&&parentIdles===0"),
            Value::Bool(true)
        ));
        let child_result = engine
            .eval_value_in_host_realm(
                &child,
                "childFrames===1&&childIdles===1&&correctThis",
                false,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(child_result, Value::Bool(true)));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(run_idle_callbacks(&mut engine, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "parentFrames===1&&parentIdles===1"),
            Value::Bool(true)
        ));

        assert!(engine.eval_value_in_host_realm(&child, "requestAnimationFrame(()=>childFrames++);requestIdleCallback(()=>childIdles++,{timeout:0})", false).unwrap().is_ok());
        engine
            .ctx()
            .with_host_realm(&child, |ctx| {
                queue_task(ctx, |_| panic!("retired child task must be dropped"))
            })
            .unwrap()
            .unwrap();
        queue_task(engine.ctx(), |ctx| {
            let global = ctx.global_object();
            ctx.member_set(&global, "parentTaskSurvived", Value::Bool(true))
                .map_err(OpError::thrown)
        })
        .unwrap();
        assert_eq!(cancel_tasks_for_realm(engine.ctx(), &child), 3);
        assert_eq!(cancel_tasks_for_realm(engine.ctx(), &child), 0);
        assert!(engine
            .ctx()
            .with_host_realm(&child, scheduler)
            .unwrap()
            .is_none());
        assert!(run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "parentTaskSurvived===true"),
            Value::Bool(true)
        ));
        assert!(engine.ctx().current_host_realm().same_realm(&parent));
        let result = engine
            .eval_value_in_host_realm(&child, "childFrames===1&&childIdles===1", false)
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn retirement_during_a_task_keeps_new_admissions_for_the_next_turn() {
        let mut engine = lumen::Engine::new();
        let child = engine.ctx().create_host_realm();
        let retired = child.clone();
        queue_task(engine.ctx(), move |ctx| {
            assert_eq!(cancel_tasks_for_realm(ctx, &retired), 1);
            queue_task(ctx, |ctx| {
                let global = ctx.global_object();
                ctx.member_set(&global, "laterTaskRan", Value::Bool(true))
                    .map_err(OpError::thrown)
            })
        })
        .unwrap();
        engine
            .ctx()
            .with_host_realm(&child, |ctx| {
                queue_task(ctx, |_| panic!("canceled child task ran"))
            })
            .unwrap()
            .unwrap();
        assert!(run_tasks(&mut engine, 2).is_empty());
        assert!(task_pending(engine.ctx()));
        assert!(matches!(
            eval(&mut engine, "typeof laterTaskRan === 'undefined'"),
            Value::Bool(true)
        ));
        assert!(run_tasks(&mut engine, 1).is_empty());
        assert!(matches!(
            eval(&mut engine, "laterTaskRan === true"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn adopted_task_survives_source_retirement_without_changing_queue_position() {
        let mut engine = lumen::Engine::new();
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let order = Rc::new(RefCell::new(Vec::new()));
        let captured = order.clone();
        let mut task = None;
        engine.ctx().with_host_realm(&child, |ctx| {
            task = Some(task_sender(ctx).unwrap().queue_tracked_when_ready(
                Rc::new(Cell::new(true)), move |_| { captured.borrow_mut().push(1); Ok(()) },
            ).ok().unwrap());
            Ok::<(), OpError>(())
        }).unwrap().unwrap();
        let captured = order.clone();
        queue_task(engine.ctx(), move |_| { captured.borrow_mut().push(2); Ok(()) }).unwrap();
        let sender = task_sender(engine.ctx()).unwrap();
        task.unwrap().retarget(&sender).unwrap();
        assert_eq!(cancel_tasks_for_realm(engine.ctx(), &child), 0);
        assert!(run_tasks(&mut engine, 16).is_empty());
        assert_eq!(*order.borrow(), vec![1, 2]);
        assert!(engine.ctx().current_host_realm().same_realm(&parent));
    }

    #[test]
    fn user_agent_tasks_enter_their_enqueue_realm_and_restore_the_host() {
        let mut engine = lumen::Engine::new();
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let parent_global = parent.global();
        let child_global = child.global();
        engine
            .ctx()
            .with_host_realm(&child, |ctx| {
                queue_task(ctx, |ctx| {
                    let object = ctx.new_object();
                    let global = ctx.global_object();
                    ctx.member_set(&global, "taskObject", Value::Obj(object))
                        .map_err(OpError::thrown)?;
                    Err(OpError::new("Error", "child task error"))
                })
                .expect("queue child task");
            })
            .expect("enter child");
        queue_task(engine.ctx(), |ctx| {
            let global = ctx.global_object();
            ctx.member_set(&global, "parentTaskRan", Value::Bool(true))
                .map_err(OpError::thrown)
        })
        .expect("queue parent task");
        let errors = run_tasks(&mut engine, 2);
        assert_eq!(errors.len(), 1);
        let restored_global = engine.ctx().global_object();
        let restored_address = engine.ctx().object_addr(&restored_global);
        assert_eq!(restored_address, engine.ctx().object_addr(&parent_global));
        let child_object = engine
            .ctx()
            .member_get(&child_global, "taskObject")
            .ok()
            .expect("child object");
        let child_prototype = engine
            .ctx()
            .member_get(&child_global, "Object")
            .ok()
            .and_then(|constructor| engine.ctx().member_get(&constructor, "prototype").ok())
            .expect("child Object prototype");
        let object_prototype = engine.ctx().prototype_of(&child_object);
        let object_address = engine.ctx().object_addr(&object_prototype);
        assert_eq!(object_address, engine.ctx().object_addr(&child_prototype));
        assert!(matches!(
            engine.ctx().member_get(&parent_global, "parentTaskRan"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            engine.ctx().member_get(&parent_global, "taskObject"),
            Ok(Value::Undefined)
        ));
    }

    #[test]
    fn user_agent_tasks_checkpoint_snapshot_and_preserve_queue_after_errors() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];Promise.resolve().then(()=>log.push('before'))",
        );
        let first = JsFunction::from_value(eval(
            &mut engine,
            "(()=>{log.push('first');Promise.resolve().then(()=>log.push('micro'))})",
        ))
        .unwrap();
        let later = JsFunction::from_value(eval(&mut engine, "(()=>log.push('later'))")).unwrap();
        let last = JsFunction::from_value(eval(&mut engine, "(()=>log.push('last'))")).unwrap();
        queue_task(engine.ctx(), move |ctx| {
            first.call(ctx, Value::Undefined, &[])?;
            queue_task(ctx, move |ctx| {
                later.call(ctx, Value::Undefined, &[]).map(|_| ())
            })
        })
        .unwrap();
        queue_task(engine.ctx(), |_| Err(OpError::new("Error", "task failure"))).unwrap();
        queue_task(engine.ctx(), move |ctx| {
            last.call(ctx, Value::Undefined, &[]).map(|_| ())
        })
        .unwrap();
        assert_eq!(idle_delay_ms(engine.ctx()), Some(0));
        assert_eq!(idle_timeout_delay_ms(engine.ctx()), Some(0));
        assert_eq!(run_tasks(&mut engine, 3).len(), 1);
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='before,first,micro,last'"),
            Value::Bool(true)
        ));
        assert!(task_pending(engine.ctx()));
        assert!(run_tasks(&mut engine, 1).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='before,first,micro,last,later'"
            ),
            Value::Bool(true)
        ));
        assert!(!task_pending(engine.ctx()));
        assert_eq!(idle_delay_ms(engine.ctx()), None);
    }

    #[test]
    fn wrapped_handles_preserve_registration_order() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        scheduler(engine.ctx()).unwrap().borrow_mut().next = u32::MAX - 1;
        eval(
            &mut engine,
            "var order=[];requestAnimationFrame(()=>order.push('first'));requestAnimationFrame(()=>order.push('second'))",
        );
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(&mut engine, "order.join(',')==='first,second'"),
            Value::Bool(true)
        ));
        scheduler(engine.ctx()).unwrap().borrow_mut().next = u32::MAX - 1;
        eval(
            &mut engine,
            "order=[];requestIdleCallback(()=>order.push('first'),{timeout:0});requestIdleCallback(()=>order.push('second'),{timeout:0})",
        );
        assert!(run_idle_callbacks(&mut engine, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "order.join(',')==='first,second'"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn cancellation_handles_follow_webidl_unsigned_long_conversion() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(&mut engine, r#"
            var frames = 0, idles = 0, conversionCalls = 0;
            const frame = requestAnimationFrame(() => frames++);
            cancelAnimationFrame({ [Symbol.toPrimitive](hint) {
                if (hint !== 'number') throw new Error('wrong primitive hint');
                conversionCalls++; return frame + 4294967296;
            }});
            const idle = requestIdleCallback(() => idles++);
            cancelIdleCallback(String(idle));
            const retained = requestAnimationFrame(() => frames++);
            const failure = new Error('conversion');
            try { cancelAnimationFrame({valueOf() {throw failure}}); throw new Error('did not throw'); }
            catch (error) { if (error !== failure) throw error; }
            cancelAnimationFrame(NaN);
            cancelAnimationFrame(Infinity);
            cancelAnimationFrame(null);
            cancelIdleCallback(undefined);
        "#);
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(run_idle_callbacks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine,
            "frames===1 && idles===0 && conversionCalls===1"), Value::Bool(true)));
    }

    #[test]
    fn animation_callbacks_snapshot_cancel_reenter_and_checkpoint() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];var times=[];requestAnimationFrame(t=>{log.push('a');times.push(t);cancelAnimationFrame(cancelled);requestAnimationFrame(()=>log.push('next'));Promise.resolve().then(()=>log.push('micro'))});var cancelled=requestAnimationFrame(()=>log.push('cancelled'));requestAnimationFrame(t=>{log.push('b');times.push(t)})",
        );
        assert!(animation_frame_pending(engine.ctx()));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='a,micro,b' && times[0]===times[1]"
            ),
            Value::Bool(true)
        ));
        assert!(run_animation_frame(&mut engine).is_empty());
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='a,micro,b,next'"),
            Value::Bool(true)
        ));
        assert!(!animation_frame_pending(engine.ctx()));
    }

    #[test]
    fn callback_failure_does_not_drop_following_callbacks() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var ran=false;requestAnimationFrame(()=>{throw new Error('frame')});requestAnimationFrame(()=>ran=true)",
        );
        assert_eq!(run_animation_frame(&mut engine).len(), 1);
        assert!(matches!(eval(&mut engine, "ran"), Value::Bool(true)));
    }

    #[test]
    fn idle_budget_timeout_cancel_and_reentrant_requests() {
        let mut engine = lumen::Engine::new();
        assert!(install(engine.ctx()).is_ok());
        eval(
            &mut engine,
            "var log=[];var cancelled=requestIdleCallback(()=>log.push('cancelled'));cancelIdleCallback(cancelled);requestIdleCallback(d=>{log.push('timeout:'+d.didTimeout+':'+d.timeRemaining());requestIdleCallback(()=>log.push('later'))},{timeout:0});requestIdleCallback(d=>log.push('idle:'+d.didTimeout))",
        );
        assert!(run_idle_callbacks(&mut engine, 0).is_empty());
        assert!(matches!(
            eval(&mut engine, "log.join(',')==='timeout:true:0'"),
            Value::Bool(true)
        ));
        assert!(run_idle_callbacks(&mut engine, 50).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "log.join(',')==='timeout:true:0,idle:false,later'"
            ),
            Value::Bool(true)
        ));
    }
}
