use super::{Limits, Parcel};
use crate::{Engine, embed::Completer};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuHint {
    Any,
    Efficiency,
    Performance,
}
impl CpuHint {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Efficiency => "efficiency",
            Self::Performance => "performance",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub core: u32,
    pub class: CpuHint,
    pub fallback: bool,
}
#[derive(Debug)]
pub struct SpawnError(pub String);
impl std::fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
impl std::error::Error for SpawnError {}

pub trait ParallelHost: Send + Sync + 'static {
    fn spawn(&self, cpu: CpuHint, job: Job) -> Result<(Placement, TaskHandle), SpawnError>;
    /// Schedule on a different executor: the worker may be spinning in JS.
    /// A zero grace is handled directly by TaskHandle::cancel.
    fn interrupt_after(&self, task: TaskHandle, grace: Duration);
    /// Owner-local check for host resources that cannot survive a checkpoint.
    fn can_migrate(&self, _ctx: &mut crate::embed::Ctx) -> bool {
        true
    }
}

pub enum CancelReason {
    User(Parcel),
    Host(&'static str),
    RealmClosed,
    Shutdown,
}
impl CancelReason {
    pub(crate) fn description(&self) -> (&'static str, &'static str) {
        match self {
            Self::User(_) => ("task cancelled", ""),
            Self::Host(message) => (message, "ERR_TASK_CANCELLED"),
            Self::RealmClosed => ("realm closed", "ERR_REALM_CLOSED"),
            Self::Shutdown => ("system shutdown", "ERR_SHUTDOWN"),
        }
    }
}
pub(crate) enum Event {
    Message(Parcel),
    Result(Parcel, bool),
    Cancel(CancelReason),
    Closed,
    Placement(Placement),
}
#[derive(Default)]
pub(crate) struct Queues {
    pub jit_stats: crate::JitStats,
    pub incoming: VecDeque<Parcel>,
    pub outgoing: VecDeque<Event>,
    pub incoming_reserved: usize,
    pub outgoing_reserved: usize,
    pub incoming_count: usize,
    pub outgoing_count: usize,
    pub incoming_closed: bool,
    pub outgoing_closed: bool,
    pub abort: Option<Parcel>,
    pub cancel_reason: Option<(&'static str, &'static str)>,
    pub cancelled: bool,
    pub pending_result: Option<(Parcel, bool)>,
    pub completer: Option<Completer>,
    pub waker: Option<Arc<dyn Fn() + Send + Sync>>,
}
pub(crate) struct Link {
    pub id: u64,
    pub host: Arc<dyn ParallelHost>,
    pub limits: Limits,
    pub queues: Mutex<Queues>,
    pub interrupt: Arc<AtomicBool>,
    pub finished: AtomicBool,
    pub parent_alive: AtomicBool,
}
impl Link {
    pub(crate) fn new(host: Arc<dyn ParallelHost>, limits: Limits) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            host,
            limits,
            queues: Mutex::new(Queues::default()),
            interrupt: Arc::new(AtomicBool::new(false)),
            finished: AtomicBool::new(false),
            parent_alive: AtomicBool::new(true),
        })
    }
    pub(crate) fn wake(&self) {
        let wake = self.queues.lock().unwrap().waker.clone();
        if let Some(wake) = wake {
            wake();
        }
    }
    pub(crate) fn notify(self: &Arc<Self>) {
        let completer = self.queues.lock().unwrap().completer.take();
        if let Some(completer) = completer {
            let link = self.clone();
            completer.settle_with(move |interp| {
                super::api::drain(interp, &link);
                Ok(crate::value::Value::Undefined)
            });
        }
    }
    pub(crate) fn push(self: &Arc<Self>, event: Event) {
        if self.parent_alive.load(Ordering::Acquire) {
            self.queues.lock().unwrap().outgoing.push_back(event);
            self.notify();
        }
    }
    pub(crate) fn parent_closed(self: &Arc<Self>) {
        self.parent_alive.store(false, Ordering::Release);
        {
            let mut queues = self.queues.lock().unwrap();
            queues.completer = None;
            queues.outgoing.clear();
        }
        let task = TaskHandle(self.clone());
        task.cancel(CancelReason::RealmClosed, Duration::ZERO);
        task.hard_stop();
    }
}

/// Reserves queue capacity before getters run or transferables are detached.
/// Reentrant postMessage calls count against the same reservation budget.
pub(crate) struct Reservation {
    link: Arc<Link>,
    outgoing: bool,
}
impl Reservation {
    pub(crate) fn new(link: &Arc<Link>, outgoing: bool) -> Result<Self, &'static str> {
        let mut queues = link.queues.lock().unwrap();
        if link.finished.load(Ordering::Acquire) || link.interrupt.load(Ordering::Acquire) {
            return Err("task is closed");
        }
        let (length, reserved, closed) = if outgoing {
            (
                queues.outgoing_count,
                queues.outgoing_reserved,
                queues.outgoing_closed,
            )
        } else {
            (
                queues.incoming_count,
                queues.incoming_reserved,
                queues.incoming_closed,
            )
        };
        if closed {
            return Err("port is closed");
        }
        if length + reserved >= link.limits.queue_depth {
            return Err("parallel message queue limit exceeded");
        }
        if outgoing {
            queues.outgoing_reserved += 1;
        } else {
            queues.incoming_reserved += 1;
        }
        Ok(Self {
            link: link.clone(),
            outgoing,
        })
    }
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let queues = self.link.queues.lock().unwrap();
        if self.link.finished.load(Ordering::Acquire)
            || self.link.interrupt.load(Ordering::Acquire)
            || if self.outgoing {
                queues.outgoing_closed
            } else {
                queues.incoming_closed
            }
        {
            Err("port is closed")
        } else {
            Ok(())
        }
    }
    pub(crate) fn send(self, parcel: Parcel) {
        {
            let mut queues = self.link.queues.lock().unwrap();
            if self.link.finished.load(Ordering::Acquire)
                || self.link.interrupt.load(Ordering::Acquire)
                || if self.outgoing {
                    queues.outgoing_closed
                } else {
                    queues.incoming_closed
                }
            {
                return;
            }
            if self.outgoing {
                queues.outgoing_count += 1;
                queues.outgoing.push_back(Event::Message(parcel));
            } else {
                queues.incoming_count += 1;
                queues.incoming.push_back(parcel);
            }
        }
        if self.outgoing {
            self.link.notify();
        } else {
            self.link.wake();
        }
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut queues = self.link.queues.lock().unwrap();
        if self.outgoing {
            queues.outgoing_reserved -= 1;
        } else {
            queues.incoming_reserved -= 1;
        }
    }
}

#[derive(Clone)]
pub struct TaskHandle(pub(crate) Arc<Link>);
impl TaskHandle {
    pub fn cancel(&self, reason: CancelReason, grace: Duration) {
        {
            let mut queues = self.0.queues.lock().unwrap();
            if queues.cancelled || self.is_finished() {
                return;
            }
            queues.cancelled = true;
            queues.incoming_closed = true;
            queues.cancel_reason = if matches!(reason, CancelReason::User(_)) {
                None
            } else {
                Some(reason.description())
            };
        }
        self.0.push(Event::Cancel(reason));
        self.0.wake();
        if grace.is_zero() {
            self.hard_stop();
        } else {
            self.0.host.interrupt_after(self.clone(), grace);
        }
    }
    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
    /// Whether the host requested a hard interrupt, including after owner cleanup.
    pub fn is_interrupted(&self) -> bool {
        self.0.interrupt.load(Ordering::Acquire)
    }
    pub fn hard_stop(&self) {
        if !self.is_finished() {
            self.0.interrupt.store(true, Ordering::Release);
            self.0.wake();
        }
    }
    pub fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.0.queues.lock().unwrap().waker = Some(wake);
    }
    pub fn set_placement(&self, placement: Placement) {
        self.0
            .queues
            .lock()
            .unwrap()
            .outgoing
            .retain(|event| !matches!(event, Event::Placement(_)));
        self.0.push(Event::Placement(placement));
    }
    /// A host failed to launch a disposed realm's continuation.
    pub fn migration_failed(&self) {
        self.cancel(CancelReason::Host("realm migration failed"), Duration::ZERO);
        self.0.finished.store(true, Ordering::Release);
        self.0.push(Event::Closed);
    }
}

pub struct Job {
    pub(crate) jit_mode: crate::JitMode,
    pub(crate) link: Arc<Link>,
    pub(crate) parcel: Parcel,
    pub(crate) spawn: bool,
    pub(crate) previous_core: Option<u32>,
}
impl Job {
    pub fn handle(&self) -> TaskHandle {
        TaskHandle(self.link.clone())
    }
    pub fn previous_core(&self) -> Option<u32> {
        self.previous_core
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Done,
    Idle,
}
/// Created, driven and destroyed on its owner's thread. Engine never crosses cores.
pub struct Worker {
    engine: Option<Engine>,
    link: Arc<Link>,
    spawn: bool,
}
impl Worker {
    pub fn new(job: Job, configure: impl FnOnce(&mut Engine)) -> Self {
        let mut engine = Engine::new();
        configure(&mut engine);
        engine.set_jit_mode(job.jit_mode);
        engine.set_can_block(true);
        super::install_with_limits(&mut engine, job.link.host.clone(), job.link.limits);
        engine.set_interrupt(job.link.interrupt.clone());
        engine.ctx().host_mut::<super::api::Realm>().unwrap().worker = Some(job.handle());
        let root = engine.interp.adopt(job.parcel);
        let mode = crate::value::Value::Bool(job.spawn);
        if let Err(error) =
            super::api::invoke(&mut engine.interp, "__parallelWorkerStart", &[root, mode])
        {
            super::api::complete(&mut engine.interp, &job.link, error, true);
        }
        Self {
            engine: Some(engine),
            link: job.link,
            spawn: job.spawn,
        }
    }
    pub fn engine(&mut self) -> &mut Engine {
        self.engine.as_mut().expect("worker finished")
    }
    pub fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        TaskHandle(self.link.clone()).set_waker(wake);
    }
    /// Dispose the source realm on its owner, returning only a Send parcel.
    /// Queues, task identity, cancellation and the parent promise survive.
    pub fn take_migration(&mut self, owner: u32) -> Option<(CpuHint, Job)> {
        let engine = self.engine.as_mut()?;
        if engine.has_pending_jobs() {
            return None;
        }
        let realm = engine.interp.host_mut::<super::api::Realm>().unwrap();
        let (cpu, parcel) = realm.migration.take()?;
        if self.link.interrupt.load(Ordering::Acquire) || self.link.queues.lock().unwrap().cancelled
        {
            return None;
        }
        if engine.ctx().has_pending_async()
            || !engine.interp.pending_async_waits.is_empty()
            || !engine.interp.pending_timers.is_empty()
            || !self.link.host.can_migrate(engine.ctx())
            || !engine
                .interp
                .host_mut::<super::api::Realm>()
                .unwrap()
                .tasks
                .is_empty()
        {
            let error = engine
                .interp
                .make_error("TypeError", "realm has pending host resources");
            super::api::complete(&mut engine.interp, &self.link, error, true);
            return None;
        }
        let jit_mode = engine.interp.jit_mode;
        self.link.queues.lock().unwrap().jit_stats += engine.jit_stats();
        drop(self.engine.take());
        crate::collect_quiescent_realms();
        Some((
            cpu,
            Job {
                jit_mode,
                link: self.link.clone(),
                parcel,
                spawn: self.spawn,
                previous_core: Some(owner),
            },
        ))
    }
    pub fn turn(&mut self) -> Turn {
        if self.engine.is_none() {
            return Turn::Done;
        }
        if self.link.interrupt.load(Ordering::Acquire) {
            self.finish();
            return Turn::Done;
        }
        let (messages, closed, abort, cancel) = {
            let mut queues = self.link.queues.lock().unwrap();
            (
                queues.incoming.drain(..).collect::<Vec<_>>(),
                queues.incoming_closed,
                queues.abort.take(),
                queues.cancel_reason.take(),
            )
        };
        let engine = self.engine.as_mut().unwrap();
        if let Some(reason) = abort {
            let reason = engine.interp.adopt(reason);
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerAbort", &[reason]);
        } else if let Some((message, code)) = cancel {
            let reason = super::api::abort_error(&engine.interp, message, code);
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerAbort", &[reason]);
        }
        for parcel in messages {
            let message = engine.interp.adopt(parcel);
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerMessage", &[message]);
        }
        if closed {
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerClose", &[]);
        }
        engine.ctx().poll_async();
        for _ in 0..128 {
            if self.link.interrupt.load(Ordering::Acquire) {
                break;
            }
            if !engine.run_one_job() {
                break;
            }
        }
        if self.link.interrupt.load(Ordering::Acquire)
            || self.link.queues.lock().unwrap().pending_result.is_some()
        {
            self.finish();
            Turn::Done
        } else {
            Turn::Idle
        }
    }
    fn finish(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.link.queues.lock().unwrap().jit_stats += engine.jit_stats();
            drop(engine);
            crate::collect_quiescent_realms();
        }
        let result = {
            let mut queues = self.link.queues.lock().unwrap();
            queues.incoming.clear();
            queues.abort = None;
            queues.waker = None;
            queues.outgoing_closed = true;
            queues.pending_result.take()
        };
        self.link.finished.store(true, Ordering::Release);
        if let Some((parcel, failed)) = result {
            self.link.push(Event::Result(parcel, failed));
        } else {
            self.link.push(Event::Closed);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.engine.is_some() {
            self.finish();
        }
    }
}

#[cfg(not(target_os = "none"))]
pub struct ThreadHost {
    configure: Arc<dyn Fn(&mut Engine) + Send + Sync>,
}
#[cfg(not(target_os = "none"))]
impl Default for ThreadHost {
    fn default() -> Self {
        Self {
            configure: Arc::new(|_| {}),
        }
    }
}
#[cfg(not(target_os = "none"))]
impl ThreadHost {
    pub fn with_configure(configure: impl Fn(&mut Engine) + Send + Sync + 'static) -> Self {
        Self {
            configure: Arc::new(configure),
        }
    }
}
#[cfg(not(target_os = "none"))]
impl ParallelHost for ThreadHost {
    fn spawn(&self, cpu: CpuHint, job: Job) -> Result<(Placement, TaskHandle), SpawnError> {
        let handle = job.handle();
        let configure = self.configure.clone();
        static NEXT_CORE: AtomicU64 = AtomicU64::new(1);
        let core = u32::try_from(NEXT_CORE.fetch_add(1, Ordering::Relaxed)).unwrap_or(u32::MAX);
        let placement = Placement {
            core,
            class: CpuHint::Any,
            fallback: cpu != CpuHint::Any,
        };
        handle.set_placement(placement);
        let host = job.link.host.clone();
        std::thread::Builder::new()
            .name(format!("lumen-parallel-{core}"))
            .stack_size(8 << 20)
            .spawn(move || {
                crate::set_thread_stack_size(8 << 20);
                let owner = std::thread::current();
                let mut worker = Worker::new(job, |engine| configure(engine));
                worker.set_waker(Arc::new(move || owner.unpark()));
                while worker.turn() != Turn::Done {
                    if let Some((cpu, job)) = worker.take_migration(placement.core) {
                        if host.spawn(cpu, job).is_err() {
                            TaskHandle(worker.link.clone()).migration_failed();
                        }
                        break;
                    }
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            })
            .map_err(|error| SpawnError(error.to_string()))?;
        Ok((placement, handle))
    }
    fn interrupt_after(&self, task: TaskHandle, grace: Duration) {
        let fallback = task.clone();
        if std::thread::Builder::new()
            .name("lumen-cancel".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                while !task.is_finished() {
                    let remaining = grace.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        break;
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                task.hard_stop();
            })
            .is_err()
        {
            fallback.hard_stop();
        }
    }
}
