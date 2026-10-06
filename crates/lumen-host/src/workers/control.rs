//! The control channel of one realm-side worker object: what a backend tells a `Worker`, a
//! `SharedWorker` or a shared worker's global about the other side.
//!
//! A [`Control`] is a thread-safe mailbox woken through the owning realm's completion channel
//! (the waker behind every port endpoint, [`crate::ports::Mailbox`]). Nothing is armed while the
//! mailbox is empty, and a burst of events costs one wake.

use crate::ports::{Mailbox, PortTransfer};
use crate::{Ctx, OpError, TaskId, Value};
use lumen_os::channel::Pop;
use std::sync::Arc;

/// What happened on the far side of a worker, as the near side's realm sees it.
#[non_exhaustive]
pub enum WorkerEvent {
    /// The worker realm is up (node's `online`; web workers ignore it).
    Online,
    /// An uncaught error in the worker, as the message `ErrorEvent` carries.
    Error(String),
    /// The worker's realm is gone; the code is node's exit code.
    Exit(i32),
    /// The connection is over: a `SharedWorker`'s `close` event, or a shared worker's wake to stop.
    Close,
    /// A client connected to a shared worker: the worker-side end of its port.
    Connect(PortTransfer),
}

/// A `Send` handle on one realm-side object's event queue.
#[derive(Clone)]
pub struct Control(Arc<Mailbox<WorkerEvent>>);

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

impl Control {
    pub fn new() -> Self {
        Self(Arc::new(Mailbox::new()))
    }

    /// Queue `event` and wake the realm; `false` when the control is closed.
    pub fn send(&self, event: WorkerEvent) -> bool {
        if self.0.queue.push(event).is_err() {
            return false;
        }
        self.0.wake();
        true
    }

    /// No further events are accepted; the realm sees the ones already queued and then the end.
    pub fn close(&self) {
        self.0.queue.close();
        self.0.wake();
    }

    pub fn is_closed(&self) -> bool {
        self.0.queue.is_closed()
    }

    /// Wake the realm without an event (a worker loop that must look at its stop flag).
    pub fn nudge(&self) {
        self.0.wake();
    }

    pub(crate) fn pop(&self) -> Pop<WorkerEvent> {
        self.0.queue.pop()
    }

    pub(crate) fn rewake(&self) {
        self.0.rewake();
    }

    pub(crate) fn listen(&self, ctx: &mut Ctx, callback: Value) -> Result<TaskId, OpError> {
        self.0.listen(ctx, callback)
    }

    pub(crate) fn unlisten(&self, ctx: &mut Ctx, task: TaskId) {
        self.0.unlisten(ctx, task);
    }
}
