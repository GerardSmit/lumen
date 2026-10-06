use super::park::OsPark;
use super::timer::Driver;
use super::{
    host_parallelism, Job, JoinThread, Park, SchedError, Scheduler, ThreadHandle, ThreadMain,
    ThreadSpec, ThreadStart, Timer,
};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// The scheduler on top of operating-system threads. The blocking pool and every other helper is
/// created on first use; an idle process holds no thread and no timer for it.
///
/// `after` runs its jobs on one `lumen-driver` thread that is started by the first call; see the
/// `timer` module. Jobs there must be short.
pub struct OsScheduler {
    pool: OnceLock<Arc<Pool>>,
    driver: OnceLock<Arc<Driver>>,
    driver_start: Mutex<()>,
}

impl OsScheduler {
    pub const fn new() -> Self {
        Self { pool: OnceLock::new(), driver: OnceLock::new(), driver_start: Mutex::new(()) }
    }

    fn pool(&self) -> &Arc<Pool> {
        self.pool.get_or_init(|| {
            Arc::new(Pool {
                state: Mutex::new(PoolState::default()),
                work: Condvar::new(),
                cap: host_parallelism().get().max(4),
            })
        })
    }

    fn driver(&self) -> Result<&Arc<Driver>, SchedError> {
        if let Some(driver) = self.driver.get() {
            return Ok(driver);
        }
        let _starting = self.driver_start.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(driver) = self.driver.get() {
            return Ok(driver);
        }
        let driver = Driver::start()?;
        Ok(self.driver.get_or_init(|| driver))
    }

    /// Whether the timer driver thread has been started.
    pub fn driver_started(&self) -> bool {
        self.driver.get().is_some()
    }

    /// Blocking-pool workers created so far.
    pub fn blocking_workers(&self) -> usize {
        self.pool.get().map_or(0, |p| p.lock().workers)
    }

    /// Blocking-pool workers currently waiting for a job.
    pub fn blocking_idle(&self) -> usize {
        self.pool.get().map_or(0, |p| p.lock().idle)
    }
}

impl Default for OsScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OsScheduler {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.get() {
            driver.shutdown();
        }
        if let Some(pool) = self.pool.get() {
            pool.lock().shutdown = true;
            pool.work.notify_all();
        }
    }
}

#[derive(Default)]
struct PoolState {
    queue: VecDeque<Job>,
    workers: usize,
    idle: usize,
    shutdown: bool,
}

struct Pool {
    state: Mutex<PoolState>,
    work: Condvar,
    cap: usize,
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn worker(self: Arc<Self>) {
        let mut state = self.lock();
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                let _ = catch_unwind(AssertUnwindSafe(job));
                state = self.lock();
                continue;
            }
            if state.shutdown {
                state.workers -= 1;
                return;
            }
            state.idle += 1;
            state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
            state.idle -= 1;
        }
    }
}

struct OsJoin(std::thread::JoinHandle<()>);

impl JoinThread for OsJoin {
    fn join(self: Box<Self>) -> Result<(), SchedError> {
        self.0.join().map_err(|_| SchedError::Exhausted("thread panicked".into()))
    }
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Scheduler for OsScheduler {
    fn name(&self) -> &'static str {
        "os"
    }

    fn available_parallelism(&self) -> NonZeroUsize {
        host_parallelism()
    }

    fn spawn_thread(&self, spec: ThreadSpec, main: ThreadMain) -> Result<ThreadHandle, SchedError> {
        let stack_bytes = spec.stack_bytes;
        let mut builder = std::thread::Builder::new().name(spec.name.into_owned());
        if stack_bytes > 0 {
            builder = builder.stack_size(stack_bytes);
        }
        let handle = builder
            .spawn(move || main(ThreadStart { stack_bytes, core: None }))
            .map_err(|e| SchedError::Os(e.into()))?;
        Ok(ThreadHandle(Box::new(OsJoin(handle))))
    }

    /// Runs `job` on an idle pool worker. A worker is created only when every existing one is
    /// busy and the pool is below `max(4, parallelism)`; workers wait on a condition variable
    /// and never time out.
    fn spawn_blocking(&self, job: Job) -> Result<(), Job> {
        let pool = self.pool();
        let mut state = pool.lock();
        if state.shutdown {
            return Err(job);
        }
        state.queue.push_back(job);
        if state.idle >= state.queue.len() {
            drop(state);
            pool.work.notify_one();
            return Ok(());
        }
        if state.workers >= pool.cap {
            return Ok(());
        }
        state.workers += 1;
        drop(state);
        let worker = pool.clone();
        let spawned = std::thread::Builder::new()
            .name("lumen-blocking".into())
            .spawn(move || worker.worker());
        if spawned.is_err() {
            let mut state = pool.lock();
            state.workers -= 1;
            if state.workers == 0 {
                if let Some(job) = state.queue.pop_back() {
                    return Err(job);
                }
            }
        }
        Ok(())
    }

    fn parker(&self) -> Result<Arc<dyn Park>, SchedError> {
        Ok(Arc::new(OsPark::new()))
    }

    fn after(&self, delay: Duration, fire: Job) -> Result<Timer, SchedError> {
        Ok(Timer::new(self.driver()?.after(delay, fire)?))
    }
}
