use super::{Limits, Parcel};
use crate::{embed::Completer, Engine};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use lumen_os::sched::{Park, Unpark};

pub use lumen_os::sched::CpuHint;
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
    /// The scheduler timer that will hard-stop the task; cancelled when the task finishes.
    pub hard_stop_timer: Mutex<Option<lumen_os::sched::Timer>>,
    #[cfg(feature = "aot-native")]
    pub native_glue: Mutex<Option<&'static [u8]>>,
}
impl Link {
    pub(crate) fn new(host: Arc<dyn ParallelHost>, limits: Limits) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            host,
            limits,
            #[cfg(feature = "aot-native")]
            native_glue: Mutex::new(None),
            queues: Mutex::new(Queues::default()),
            interrupt: Arc::new(AtomicBool::new(false)),
            finished: AtomicBool::new(false),
            parent_alive: AtomicBool::new(true),
            hard_stop_timer: Mutex::new(None),
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
fn install_worker_glue(engine: &mut Engine, link: &Arc<Link>) -> Result<(), String> {
    #[cfg(feature = "compiler")]
    {
        super::install_with_limits(engine, link.host.clone(), link.limits);
        Ok(())
    }
    #[cfg(not(feature = "compiler"))]
    {
        let _ = (engine, link);
        Err("native parallel glue is unavailable".into())
    }
}
impl Worker {
    pub fn new(job: Job, configure: impl FnOnce(&mut Engine)) -> Self {
        let mut engine = Engine::new();
        configure(&mut engine);
        engine.set_jit_mode(job.jit_mode);
        engine.set_can_block(true);
        #[cfg(feature = "aot-native")]
        let native_glue = job.link.native_glue.lock().unwrap().clone();
        #[cfg(feature = "aot-native")]
        let installed = if let Some(bytes) = native_glue {
            super::install_native_with_limits(
                &mut engine,
                job.link.host.clone(),
                job.link.limits,
                bytes,
            )
        } else {
            install_worker_glue(&mut engine, &job.link)
        };
        #[cfg(not(feature = "aot-native"))]
        let installed = install_worker_glue(&mut engine, &job.link);
        if let Err(message) = installed {
            let error = engine.interp.make_error("EvalError", &message);
            super::api::complete(&mut engine.interp, &job.link, error, true);
            let mut worker = Self {
                engine: Some(engine),
                link: job.link,
                spawn: job.spawn,
            };
            worker.finish();
            return worker;
        }
        engine.set_interrupt(job.link.interrupt.clone());
        engine.ctx().host_mut::<super::api::Realm>().unwrap().worker = Some(job.handle());
        let mode = crate::value::Value::Bool(job.spawn);
        let started = engine.interp.try_adopt(job.parcel).and_then(|root| {
            super::api::invoke(&mut engine.interp, "__parallelWorkerStart", &[root, mode])
        });
        if let Err(error) = started {
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
            let reason = engine
                .interp
                .try_adopt(reason)
                .unwrap_or_else(|error| error);
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerAbort", &[reason]);
        } else if let Some((message, code)) = cancel {
            let reason = super::api::abort_error(&engine.interp, message, code);
            let _ = super::api::invoke(&mut engine.interp, "__parallelWorkerAbort", &[reason]);
        }
        for parcel in messages {
            match engine.interp.try_adopt(parcel) {
                Ok(message) => {
                    let _ = super::api::invoke(
                        &mut engine.interp,
                        "__parallelWorkerMessage",
                        &[message],
                    );
                }
                Err(error) => super::api::complete(&mut engine.interp, &self.link, error, true),
            }
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
    #[cfg(not(target_os = "none"))]
    fn fail(&mut self, message: &str) {
        let link = self.link.clone();
        let error = self.engine().interp.make_error("EvalError", message);
        super::api::complete(&mut self.engine().interp, &link, error, true);
        self.finish();
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
        self.link.hard_stop_timer.lock().unwrap().take();
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
type Configure = Arc<dyn Fn(&mut Engine, Arc<dyn Unpark>) + Send + Sync>;
#[cfg(not(target_os = "none"))]
type TurnFn = Arc<dyn Fn(&mut Engine) -> Result<Option<Instant>, String> + Send + Sync>;

#[cfg(not(target_os = "none"))]
const THREAD_HOST_STACK: usize = 8 << 20;

/// Runs each task on a scheduler thread. The thread parks on the scheduler between turns and is
/// woken by messages, cancellation and (through the unparker `configure` receives) the realm's
/// own completions; nothing is armed while it is idle.
#[cfg(not(target_os = "none"))]
pub struct ThreadHost {
    configure: Configure,
    turn: TurnFn,
    can_migrate: bool,
}
#[cfg(not(target_os = "none"))]
impl Default for ThreadHost {
    fn default() -> Self {
        Self {
            configure: Arc::new(|_, _| {}),
            turn: Arc::new(|_| Ok(None)),
            can_migrate: true,
        }
    }
}
#[cfg(not(target_os = "none"))]
impl ThreadHost {
    pub fn with_configure(configure: impl Fn(&mut Engine) + Send + Sync + 'static) -> Self {
        Self {
            configure: Arc::new(move |engine, _| configure(engine)),
            turn: Arc::new(|_| Ok(None)),
            can_migrate: true,
        }
    }

    /// A host with its own event loop in the realm. `configure` receives an unparker to call
    /// whenever the loop gets work from another thread. `turn` runs everything that is ready and
    /// returns when it next needs to run on its own (the earliest timer), or `None` to wait for
    /// an unpark.
    pub fn with_turn(
        configure: impl Fn(&mut Engine, Arc<dyn Unpark>) + Send + Sync + 'static,
        turn: impl Fn(&mut Engine) -> Result<Option<Instant>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            configure: Arc::new(configure),
            turn: Arc::new(turn),
            can_migrate: false,
        }
    }
}
#[cfg(not(target_os = "none"))]
impl ParallelHost for ThreadHost {
    fn can_migrate(&self, _: &mut crate::embed::Ctx) -> bool {
        self.can_migrate
    }
    fn spawn(&self, cpu: CpuHint, job: Job) -> Result<(Placement, TaskHandle), SpawnError> {
        let handle = job.handle();
        let configure = self.configure.clone();
        let turn = self.turn.clone();
        static NEXT_CORE: AtomicU64 = AtomicU64::new(1);
        let core = u32::try_from(NEXT_CORE.fetch_add(1, Ordering::Relaxed)).unwrap_or(u32::MAX);
        let placement = Placement {
            core,
            class: CpuHint::Any,
            fallback: cpu != CpuHint::Any,
        };
        handle.set_placement(placement);
        let host = job.link.host.clone();
        let mut spec = lumen_os::sched::ThreadSpec::new(
            format!("lumen-parallel-{core}"),
            lumen_os::sched::Purpose::Engine,
        );
        spec.stack_bytes = THREAD_HOST_STACK;
        spec.cpu = cpu;
        lumen_os::sched::current()
            .spawn_thread(
                spec,
                Box::new(move |start| {
                    crate::set_thread_stack_size(if start.stack_bytes > 0 {
                        start.stack_bytes
                    } else {
                        THREAD_HOST_STACK
                    });
                    run_worker(job, host, configure, turn, placement.core);
                }),
            )
            .map_err(|error| SpawnError(format!("{error:?}")))?;
        Ok((placement, handle))
    }
    fn interrupt_after(&self, task: TaskHandle, grace: Duration) {
        let fired = task.clone();
        match lumen_os::sched::current().after(grace, Box::new(move || fired.hard_stop())) {
            Ok(timer) => {
                *task.0.hard_stop_timer.lock().unwrap() = Some(timer);
                if task.is_finished() {
                    task.0.hard_stop_timer.lock().unwrap().take();
                }
            }
            Err(_) => task.hard_stop(),
        }
    }
}

#[cfg(not(target_os = "none"))]
fn run_worker(
    job: Job,
    host: Arc<dyn ParallelHost>,
    configure: Configure,
    turn: TurnFn,
    core: u32,
) {
    let park: Arc<dyn Park> = lumen_os::sched::current()
        .parker()
        .unwrap_or_else(|_| Arc::new(lumen_os::sched::OsPark::new()));
    let unpark = park.clone().unparker();
    let mut worker = Worker::new(job, |engine| configure(engine, unpark.clone()));
    {
        let wake = unpark.clone();
        worker.set_waker(Arc::new(move || wake.unpark()));
        let wake = unpark.clone();
        if let Some(engine) = worker.engine.as_mut() {
            engine
                .ctx()
                .set_async_waker(Arc::new(move || wake.unpark()));
        }
    }
    loop {
        if worker.engine.is_none() {
            break;
        }
        if let Err(message) = turn(worker.engine()) {
            worker.fail(&message);
            break;
        }
        if worker.turn() == Turn::Done {
            break;
        }
        if let Some((cpu, job)) = worker.take_migration(core) {
            if host.spawn(cpu, job).is_err() {
                TaskHandle(worker.link.clone()).migration_failed();
            }
            break;
        }
        // The message handlers above may have queued work for the realm's own loop.
        let next = match turn(worker.engine()) {
            Ok(next) => next,
            Err(message) => {
                worker.fail(&message);
                break;
            }
        };
        let deadline = if worker.engine().has_pending_jobs() {
            Some(Instant::now())
        } else {
            next
        };
        park.park(deadline);
    }
}
