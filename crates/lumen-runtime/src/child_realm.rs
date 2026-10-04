//! Child realms: what `child_process` starts when an embedded program spawns its own
//! `process.execPath` (or `fork()`s). No binary on disk runs a realm, so the child is another
//! realm in this process, on a thread of its own, with the argv, environment, working directory,
//! stdio and IPC channel a child process would have had.
//!
//! The launching realm owns its children: dropping its [`Runtime`](crate::Runtime) stops and joins
//! every one still running, so whoever waits for the realm waits for them too. Children inherit
//! the parent's [`Spawner`] and live-object limit, and start children of their own the same way.
//!
//! Limits: a realm runs at most [`MAX_CHILDREN_PER_REALM`] live children and a whole tree of
//! realms (the root's children, their children, and so on) at most [`MAX_REALMS_PER_TREE`]; a
//! launch past either fails with `EAGAIN`. Every realm has the parent's live-object limit, so a
//! tree's worst case is `MAX_REALMS_PER_TREE` times that limit, plus one thread with a 256 MiB
//! reserved (not committed) stack per realm. A finished child is reaped as soon as it ends.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lumen_host::{
    ChildRealm, ChildRealmExit, ChildRealmRequest, RealmLauncher, RealmProcess, Spawner,
    REALM_PID_BASE,
};

use crate::{Embedding, RealmExit, Runtime, SharedWriter, Terminator};

/// The same stack the standalone binary gives its main thread: the engine recurses natively.
const CHILD_REALM_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Live children one realm may have at a time.
pub(crate) const MAX_CHILDREN_PER_REALM: usize = 8;
/// Live child realms, at any depth, one tree may have at a time.
pub(crate) const MAX_REALMS_PER_TREE: usize = 16;
/// How long a stopping launcher lets children handle `SIGTERM` before it stops them for good.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

/// Signals whose default action ends the program (the others are ignored, as `SIGWINCH` is).
pub(crate) fn terminates_by_default(signal: i32) -> bool {
    matches!(signal, SIGHUP | SIGINT | SIGQUIT | SIGTERM)
}

/// Child realms alive across one tree of realms.
#[derive(Default)]
pub(crate) struct RealmTree {
    live: AtomicUsize,
}

static NEXT_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(REALM_PID_BASE);

struct Child {
    pid: u32,
    interrupt: Arc<AtomicBool>,
    terminator: Mutex<Option<Terminator>>,
    handlers: Mutex<Option<Arc<AtomicU64>>>,
    signal: Mutex<Option<i32>>,
    exit: Mutex<Option<ChildRealmExit>>,
}

impl Child {
    fn has_listener(&self, signal: i32) -> bool {
        (1..64).contains(&signal)
            && self
                .handlers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|mask| mask.load(Ordering::SeqCst) & (1 << signal) != 0)
    }
}

impl ChildRealm for Child {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn terminate(&self, signal: i32) {
        if self.exit().is_some() {
            return;
        }
        self.signal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(signal);
        self.interrupt.store(true, Ordering::SeqCst);
        if let Some(terminator) = self
            .terminator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            terminator.terminate();
        }
    }

    fn signal(&self, signal: i32) {
        if self.exit().is_some() {
            return;
        }
        if signal != SIGKILL && self.has_listener(signal) {
            let terminator = self
                .terminator
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(terminator) = terminator.as_ref() {
                terminator.deliver(signal);
                return;
            }
        }
        if signal == SIGKILL || terminates_by_default(signal) {
            self.terminate(signal);
        }
    }

    fn exit(&self) -> Option<ChildRealmExit> {
        *self.exit.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

type Children = Arc<Mutex<HashMap<u32, (Arc<Child>, JoinHandle<()>)>>>;

/// The launcher of one realm: remembers its children so the realm's end can stop them.
pub(crate) struct ChildRealms {
    spawner: Option<Arc<dyn Spawner>>,
    live_object_limit: Option<i64>,
    tree: Arc<RealmTree>,
    children: Children,
}

impl ChildRealms {
    pub(crate) fn new(
        spawner: Option<Arc<dyn Spawner>>,
        live_object_limit: Option<i64>,
        tree: Arc<RealmTree>,
    ) -> Self {
        ChildRealms {
            spawner,
            live_object_limit,
            tree,
            children: Arc::default(),
        }
    }

    /// Ask every child still running to end with `SIGTERM`, stop the ones that have not after a
    /// short grace, and wait for all of them.
    pub(crate) fn shutdown(&self) {
        let children =
            std::mem::take(&mut *self.children.lock().unwrap_or_else(PoisonError::into_inner));
        for (child, _) in children.values() {
            child.signal(SIGTERM);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while children.values().any(|(child, _)| child.exit().is_none())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        for (child, _) in children.values() {
            child.terminate(SIGKILL);
        }
        for (_, (_, thread)) in children {
            let _ = thread.join();
        }
    }
}

impl RealmLauncher for ChildRealms {
    fn launch(&self, request: ChildRealmRequest) -> io::Result<Arc<dyn ChildRealm>> {
        let ChildRealmRequest {
            argv,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
            interrupt,
            owned_fds,
            resources,
            ipc: _,
        } = request;
        let script = argv
            .get(1)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no script to run"))?;
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        let live = children
            .values()
            .filter(|(child, _)| child.exit().is_none())
            .count();
        let too_many = || {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "too many child realms are running",
            )
        };
        if live >= MAX_CHILDREN_PER_REALM {
            return Err(too_many());
        }
        if self.tree.live.fetch_add(1, Ordering::SeqCst) >= MAX_REALMS_PER_TREE {
            self.tree.live.fetch_sub(1, Ordering::SeqCst);
            return Err(too_many());
        }
        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        let child = Arc::new(Child {
            pid,
            interrupt: Arc::clone(&interrupt),
            terminator: Mutex::new(None),
            handlers: Mutex::new(None),
            signal: Mutex::new(None),
            exit: Mutex::new(None),
        });
        let embedding = Embedding {
            argv,
            env,
            cwd,
            stdin,
            stdout: SharedWriter::new(stdout),
            stderr: SharedWriter::new(stderr),
            interrupt,
            live_object_limit: self.live_object_limit,
            spawner: self.spawner.clone(),
        };
        let state = Arc::clone(&child);
        let tree = Arc::clone(&self.tree);
        let registry = Arc::clone(&self.children);
        let thread = std::thread::Builder::new()
            .name("lumen-child-realm".into())
            .stack_size(CHILD_REALM_STACK_BYTES)
            .spawn(move || {
                lumen::set_thread_stack_size(CHILD_REALM_STACK_BYTES);
                let mut runtime = Runtime::new_child(embedding, Arc::clone(&tree));
                if let Some(realm) = runtime.engine().ctx().host_mut::<RealmProcess>() {
                    realm.owned_fds = owned_fds.clone();
                }
                if let Some(workers) = runtime.engine().ctx().host_mut::<crate::WorkerEmbedding>() {
                    workers.owned_fds = owned_fds;
                }
                *state
                    .handlers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = runtime.signal_handlers();
                *state
                    .terminator
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = runtime.terminator();
                let exit = if state.interrupt.load(Ordering::SeqCst) {
                    RealmExit::Terminated
                } else {
                    runtime.run_embedded_main(&script)
                };
                if let Some(signal) = runtime.signal_exit() {
                    state
                        .signal
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_or_insert(signal);
                }
                state
                    .terminator
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                // Dropping the runtime closes the realm's ends of its pipes and stops its own
                // children; only then does the parent see the exit.
                state.interrupt.store(true, Ordering::SeqCst);
                drop(runtime);
                drop(resources);
                let signal = state
                    .signal
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                let exit = match (exit, signal) {
                    (RealmExit::Exited(code), None) => ChildRealmExit::Exited(code),
                    // Crossing the heap ceiling is an abort, as an out-of-memory process is.
                    (RealmExit::HeapLimit, _) => ChildRealmExit::Signalled(6),
                    (RealmExit::Exited(_) | RealmExit::Terminated, Some(signal)) => {
                        ChildRealmExit::Signalled(signal)
                    }
                    (RealmExit::Terminated, None) => ChildRealmExit::Signalled(SIGTERM),
                };
                *state.exit.lock().unwrap_or_else(PoisonError::into_inner) = Some(exit);
                tree.live.fetch_sub(1, Ordering::SeqCst);
                // Reaped now rather than at the next launch; the thread has nothing left to do.
                registry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&state.pid);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                self.tree.live.fetch_sub(1, Ordering::SeqCst);
                return Err(error);
            }
        };
        children.insert(pid, (Arc::clone(&child), thread));
        Ok(child)
    }

    fn signal_pid(&self, pid: u32, signal: i32) -> bool {
        let child = self
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pid)
            .map(|(child, _)| Arc::clone(child));
        match child {
            Some(child) if child.exit().is_none() => {
                if signal != 0 {
                    child.signal(signal);
                }
                true
            }
            _ => false,
        }
    }
}
