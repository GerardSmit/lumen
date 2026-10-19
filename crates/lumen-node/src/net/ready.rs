//! Readiness-driven socket I/O on the loop's reactor.
//!
//! One [`Io`] belongs to one descriptor (a connected socket, a listener, a datagram socket). It
//! holds the operations that are waiting on it, one queue per direction, and arms its single
//! one-shot reactor registration for the directions that have something queued. An operation is
//! a *step*: a closure that tries its syscall without blocking and answers [`Step::Done`] with the
//! completion payload, or [`Step::Again`] when the kernel said `WouldBlock`.
//!
//! The operation is tried at once on submission, so a descriptor that is already ready never
//! touches the reactor. Only after `WouldBlock` is the registration armed (created on the first
//! need, re-armed afterwards), and the wake that follows runs on the loop thread inside its turn:
//! it reads or writes straight into the operation's destination and sends the completion. Nothing
//! polls and nothing owns a thread.
//!
//! The registration is dropped before the descriptor's owner, so the descriptor is deregistered
//! before it can be closed (and its number reused).

use lumen_host::{CompletionSender, TaskId};
use lumen_os::reactor::{Interest, Ready, Reactor, Registration, Source, Wake};
use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// Why a step runs.
pub(super) enum Trigger {
    /// The descriptor may be ready (or the operation was just submitted): try the syscall.
    Ready,
    /// The reactor could not arm the descriptor, or the descriptor is being closed: finish with
    /// this error.
    Failed(std::io::Error),
}

/// What a step reports.
pub(super) enum Step {
    /// The operation finished; the payload settles its task.
    Done(Box<dyn Any + Send>),
    /// `WouldBlock`: wait for readiness and run the step again.
    Again,
    /// The operation finished and settled its task (or handed it on) by itself.
    #[cfg_attr(not(unix), allow(dead_code))]
    Handled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    Read,
    Write,
}

type StepFn = Box<dyn FnMut(Trigger) -> Step + Send>;

struct Job {
    id: TaskId,
    step: StepFn,
}

#[derive(Default)]
struct Queues {
    read: VecDeque<Job>,
    write: VecDeque<Job>,
    /// What the registration is armed for right now; a wake disarms all of it.
    armed: Interest,
}

impl Queues {
    fn queue(&mut self, dir: Dir) -> &mut VecDeque<Job> {
        match dir {
            Dir::Read => &mut self.read,
            Dir::Write => &mut self.write,
        }
    }

    fn wanted(&self) -> Interest {
        let mut want = Interest::default();
        if !self.read.is_empty() {
            want = want | Interest::READ;
        }
        if !self.write.is_empty() {
            want = want | Interest::WRITE;
        }
        want
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct Io {
    // Field order matters: the registration goes first, the descriptor's owner last.
    registration: Mutex<Option<Registration>>,
    queues: Mutex<Queues>,
    reactor: Arc<dyn Reactor>,
    source: Source,
    completions: CompletionSender,
    me: Weak<Io>,
    _owner: Arc<dyn Any + Send + Sync>,
}

struct WakeIo(Weak<Io>);

impl Wake for WakeIo {
    fn wake(&self) {
        if let Some(io) = self.0.upgrade() {
            io.on_ready();
        }
    }
}

impl Io {
    /// An `Io` for `source`, whose descriptor `owner` keeps open for as long as this lives. No
    /// syscall is made until an operation has to wait.
    pub(super) fn new(
        reactor: Arc<dyn Reactor>,
        source: Source,
        completions: CompletionSender,
        owner: Arc<dyn Any + Send + Sync>,
    ) -> Arc<Io> {
        Arc::new_cyclic(|me| Io {
            registration: Mutex::new(None),
            queues: Mutex::new(Queues::default()),
            reactor,
            source,
            completions,
            me: me.clone(),
            _owner: owner,
        })
    }

    /// Queues `step` for task `id` in `dir`. With `try_now` it runs at once (when nothing is
    /// ahead of it); without, the first run waits for readiness.
    pub(super) fn submit(
        &self,
        dir: Dir,
        id: TaskId,
        try_now: bool,
        step: impl FnMut(Trigger) -> Step + Send + 'static,
    ) {
        let mut queues = lock(&self.queues);
        let queue = queues.queue(dir);
        queue.push_back(Job { id, step: Box::new(step) });
        if queue.len() == 1 && try_now {
            self.drain(&mut queues, dir);
        }
        self.arm(&mut queues);
    }

    /// Drops every queued read, and finishes every queued write with `error` (its task settles
    /// with the failure). The descriptor is going away.
    pub(super) fn close(&self, error: impl Fn() -> std::io::Error) {
        let mut queues = lock(&self.queues);
        queues.read.clear();
        while let Some(mut job) = queues.write.pop_front() {
            if let Step::Done(result) = (job.step)(Trigger::Failed(error())) {
                self.completions.send(job.id, result);
            }
        }
        queues.armed = Interest::default();
    }

    /// Whether a reactor registration exists.
    #[cfg(test)]
    pub(super) fn is_registered(&self) -> bool {
        lock(&self.registration).is_some()
    }

    fn drain(&self, queues: &mut Queues, dir: Dir) {
        loop {
            let queue = queues.queue(dir);
            let Some(job) = queue.front_mut() else { return };
            match (job.step)(Trigger::Ready) {
                Step::Done(result) => {
                    let id = job.id;
                    queue.pop_front();
                    self.completions.send(id, result);
                }
                Step::Handled => {
                    queue.pop_front();
                }
                Step::Again => return,
            }
        }
    }

    fn arm(&self, queues: &mut Queues) {
        let want = queues.wanted();
        if want.is_empty() || queues.armed.contains(want) {
            return;
        }
        let mut registration = lock(&self.registration);
        let armed = match registration.as_ref() {
            Some(r) => r.rearm(want),
            None => self
                .reactor
                .register(self.source, want, Arc::new(WakeIo(self.me.clone())))
                .map(|r| *registration = Some(r)),
        };
        drop(registration);
        match armed {
            Ok(()) => queues.armed = want,
            Err(e) => self.fail(queues, &e.to_string()),
        }
    }

    fn fail(&self, queues: &mut Queues, message: &str) {
        queues.armed = Interest::default();
        for dir in [Dir::Read, Dir::Write] {
            while let Some(mut job) = queues.queue(dir).pop_front() {
                let error = std::io::Error::other(message.to_string());
                if let Step::Done(result) = (job.step)(Trigger::Failed(error)) {
                    self.completions.send(job.id, result);
                }
            }
        }
    }

    fn on_ready(&self) {
        let ready = lock(&self.registration)
            .as_ref()
            .map_or(Ready::NONE, |r| r.take_ready());
        let failed = ready.contains(Ready::ERROR) || ready.contains(Ready::HUP);
        let mut queues = lock(&self.queues);
        queues.armed = Interest::default();
        if failed || ready.contains(Ready::READ) {
            self.drain(&mut queues, Dir::Read);
        }
        if failed || ready.contains(Ready::WRITE) {
            self.drain(&mut queues, Dir::Write);
        }
        self.arm(&mut queues);
    }
}

/// An in-flight nonblocking connect: the reactor registration of the address currently being
/// tried hangs off it, so dropping or cancelling the attempt deregisters the socket.
#[cfg(unix)]
pub(super) struct ConnectAttempt {
    cancelled: std::sync::atomic::AtomicBool,
    current: Mutex<Option<Arc<Io>>>,
}

#[cfg(unix)]
impl ConnectAttempt {
    pub(super) fn new() -> Arc<ConnectAttempt> {
        Arc::new(ConnectAttempt {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            current: Mutex::new(None),
        })
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Stops the attempt and drops the registration being waited on.
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        let io = lock(&self.current).take();
        drop(io);
    }

    /// Makes `io` the socket being waited on, dropping the previous one; `false` (and `io` is
    /// dropped) when the attempt was cancelled meanwhile.
    pub(super) fn set_current(&self, io: Arc<Io>) -> bool {
        let previous = {
            let mut current = lock(&self.current);
            if self.is_cancelled() {
                return false;
            }
            current.replace(io)
        };
        drop(previous);
        true
    }

    #[cfg(test)]
    pub(super) fn current(&self) -> Option<Arc<Io>> {
        lock(&self.current).clone()
    }
}

#[cfg(unix)]
impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        let io = lock(&self.current).take();
        drop(io);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::net::{read_step, NetStream};
    use lumen_host::TaskCompletion;
    use lumen_os::reactor::Poller;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    struct Rig {
        poller: Arc<Poller>,
        completions: CompletionSender,
        done: mpsc::Receiver<TaskCompletion>,
    }

    fn rig() -> Rig {
        let (tx, done) = mpsc::channel();
        Rig {
            poller: Arc::new(Poller::new().expect("poller")),
            completions: CompletionSender::new(tx),
            done,
        }
    }

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    fn io_for(rig: &Rig, stream: &Arc<NetStream>) -> Arc<Io> {
        Io::new(
            rig.poller.clone(),
            Source::Fd(stream.raw_fd()),
            rig.completions.clone(),
            stream.clone(),
        )
    }

    fn bytes_of(done: TaskCompletion) -> Vec<u8> {
        let result = done
            .result
            .downcast::<Result<Vec<u8>, crate::net::NetErr>>()
            .ok()
            .expect("read payload");
        match *result {
            Ok(bytes) => bytes,
            Err(_) => panic!("read failed"),
        }
    }

    #[test]
    fn read_that_hit_would_block_is_woken_and_rearmed_for_the_next() {
        let rig = rig();
        let (client, mut server) = pair();
        let stream = Arc::new(NetStream::Tcp(client));
        let io = io_for(&rig, &stream);

        io.submit(Dir::Read, 1, true, read_step(stream.clone()));
        assert!(io.is_registered(), "WouldBlock arms the registration");
        assert!(rig.done.try_recv().is_err());
        assert_eq!(rig.poller.turn(Some(Duration::from_millis(20))).unwrap(), 0);

        server.write_all(b"first").unwrap();
        assert_eq!(rig.poller.turn(Some(Duration::from_secs(5))).unwrap(), 1);
        let done = rig.done.try_recv().expect("completion");
        assert_eq!(done.task, 1);
        assert_eq!(bytes_of(done), b"first");

        io.submit(Dir::Read, 2, true, read_step(stream.clone()));
        assert!(rig.done.try_recv().is_err(), "nothing is queued yet");
        server.write_all(b"second").unwrap();
        assert_eq!(rig.poller.turn(Some(Duration::from_secs(5))).unwrap(), 1);
        assert_eq!(bytes_of(rig.done.try_recv().expect("completion")), b"second");
    }

    #[test]
    fn data_already_there_never_touches_the_reactor() {
        let rig = rig();
        let (client, mut server) = pair();
        server.write_all(b"ready").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let stream = Arc::new(NetStream::Tcp(client));
        let io = io_for(&rig, &stream);
        io.submit(Dir::Read, 7, true, read_step(stream.clone()));
        assert!(!io.is_registered());
        assert_eq!(bytes_of(rig.done.try_recv().expect("completion")), b"ready");
    }

    #[test]
    fn cancelling_a_connect_attempt_drops_its_registration() {
        let rig = rig();
        let (client, mut server) = pair();
        let stream = Arc::new(NetStream::Tcp(client));
        let attempt = ConnectAttempt::new();
        let io = io_for(&rig, &stream);
        assert!(attempt.set_current(io.clone()));
        io.submit(Dir::Read, 3, true, read_step(stream.clone()));
        assert!(io.is_registered());
        let weak = Arc::downgrade(&io);
        drop(io);
        assert!(attempt.current().is_some());

        attempt.cancel();
        assert!(attempt.is_cancelled());
        assert!(weak.upgrade().is_none(), "the registration went with the Io");
        assert!(!attempt.set_current(io_for(&rig, &stream)));

        server.write_all(b"late").unwrap();
        assert_eq!(rig.poller.turn(Some(Duration::from_millis(50))).unwrap(), 0);
        assert!(rig.done.try_recv().is_err());
    }
}
