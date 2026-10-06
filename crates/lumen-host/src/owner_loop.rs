//! Event delivery for a host that has no event loop of its own and instead gives each realm a
//! turn from its own scheduler (the Bitnest kernel's shell). Design: `docs/plans/native-workers.md`.
//!
//! A realm installs the loop with [`install`]; from then on everything that completes through the
//! realm's [`CompletionSender`] (port wakes, worker control events) is counted, queued on an
//! `mpsc` channel and announced to the host once. The host calls [`has_ready`] to learn whether a
//! turn is worth giving to the realm and [`pump`] to run a bounded number of completions.
//!
//! Costs. Nothing is armed while the realm is idle: no timer, no poll and no thread exists, and a
//! realm that received nothing is not pumped. A send is one atomic increment, a channel push and
//! (only on the first send after the realm drained) the `notify` call; the sender never spawns a
//! thread, so [`CompletionSender::run_blocking`] is unsupported on these senders.

use crate::{CompletionSender, Ctx, Engine, OpState, TaskCompletion, TaskRegistry, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

/// The counter and wake-up hook shared by every clone of an owner-loop [`CompletionSender`] and
/// the realm's receiving end.
pub struct Wake {
    pending: AtomicUsize,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Wake {
    pub(crate) fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            notify,
        }
    }

    /// Count one completion; `true` when it is the first since the realm drained.
    pub(crate) fn arrive(&self) -> bool {
        self.pending.fetch_add(1, Ordering::AcqRel) == 0
    }

    pub(crate) fn notify(&self) {
        (self.notify)();
    }

    fn ready(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0
    }

    fn consumed(&self) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The receiving end, kept in the realm's [`OpState`].
struct OwnerLoop {
    rx: mpsc::Receiver<TaskCompletion>,
    wake: Arc<Wake>,
}

/// Give the realm a [`TaskRegistry`] (when it has none) and an owner-loop [`CompletionSender`].
/// `notify` is called on the sending thread when the realm goes from nothing ready to something
/// ready; the kernel passes its shell waker. Calling it again replaces the loop.
pub fn install(ctx: &mut Ctx, notify: Arc<dyn Fn() + Send + Sync>) {
    let (tx, rx) = mpsc::channel();
    let sender = CompletionSender::with_notify(tx, notify);
    let wake = sender.wake_handle().expect("owner-loop sender counts completions");
    let state: &mut OpState = ctx.op_state();
    if !state.has::<TaskRegistry>() {
        state.put(TaskRegistry::default());
    }
    state.put(sender);
    state.put(OwnerLoop { rx, wake });
}

/// Whether a completion is waiting. A counter read: no syscall, no lock, no timer.
pub fn has_ready(ctx: &mut Ctx) -> bool {
    ctx.op_state()
        .get::<OwnerLoop>()
        .is_some_and(|owner| owner.wake.ready())
}

/// What a completion asks the loop to do once it is [`settle`]d.
pub enum Outcome {
    /// Call `callback(...args)`.
    Call { callback: Value, args: Vec<Value> },
    /// The decoder failed and the task has no failure callback: report the value as uncaught.
    Uncaught(Value),
}

/// A decoded completion. Call [`Settled::finish`] after running the outcome so the async context
/// of the caller is restored.
pub struct Settled {
    pub outcome: Outcome,
    outer: Value,
}

impl Settled {
    pub fn finish(self, ctx: &mut Ctx) {
        ctx.set_async_context(self.outer);
    }
}

/// Look up the task `done` completes and decode its payload inside the async context the task
/// was admitted with. `None` when the task was cancelled while the completion was in flight.
pub fn settle(ctx: &mut Ctx, done: TaskCompletion) -> Option<Settled> {
    let entry = ctx
        .host_mut::<TaskRegistry>()
        .and_then(|registry| registry.take(done.task))?;
    let outer = ctx.set_async_context(entry.context);
    let outcome = match (entry.decode)(ctx, done.result) {
        Ok(args) => Outcome::Call {
            callback: entry.on_ok,
            args,
        },
        Err(error) => match entry.on_err {
            Some(reject) => Outcome::Call {
                callback: reject,
                args: vec![error],
            },
            None => Outcome::Uncaught(error),
        },
    };
    Some(Settled { outcome, outer })
}

/// Run up to `budget` completions, with a microtask checkpoint after each. Returns the values
/// that were thrown (by callbacks, by decoders without a failure callback and by microtasks); the
/// host reports them. Completions beyond `budget` stay queued and [`has_ready`] stays true, so the
/// host schedules another turn.
///
/// A completion stays counted while it runs, so what a callback posts during the pump (the next
/// message of the same port) does not notify the host again: the host checks [`has_ready`] when
/// the pump returns.
pub fn pump(engine: &mut Engine, budget: usize) -> Vec<Value> {
    let mut thrown = Vec::new();
    let Some(wake) = engine
        .ctx()
        .op_state()
        .get::<OwnerLoop>()
        .map(|owner| Arc::clone(&owner.wake))
    else {
        return thrown;
    };
    for _ in 0..budget {
        let Some(done) = next(engine.ctx()) else {
            break;
        };
        if let Some(settled) = settle(engine.ctx(), done) {
            match &settled.outcome {
                Outcome::Call { callback, args } => {
                    if let Err(error) = engine.call_function(callback, Value::Undefined, args) {
                        thrown.push(error);
                    }
                }
                Outcome::Uncaught(error) => thrown.push(error.clone()),
            }
            settled.finish(engine.ctx());
            engine.run_microtasks();
            thrown.extend(engine.take_task_errors());
        }
        wake.consumed();
    }
    thrown
}

fn next(ctx: &mut Ctx) -> Option<TaskCompletion> {
    ctx.op_state().get::<OwnerLoop>()?.rx.try_recv().ok()
}
