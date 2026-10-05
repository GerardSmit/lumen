//! lumen-timers — the timer globals (`setTimeout`, `setInterval`, `clearTimeout`,
//! `clearInterval`, `setImmediate`) as an op crate.
//!
//! The ops only mutate the [`Timers`] heap in `OpState`; nothing here sleeps, spawns, or
//! fires. The runtime's event loop drives everything: it asks [`Timers::next_deadline`] how
//! long it may block, and fires [`Timers::take_due`] callbacks each turn. `setImmediate`
//! doesn't touch the heap at all — it queues on the loop's [`CallbackQueue`] for the next
//! turn.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::Duration;

#[cfg(feature = "hosted")]
use lumen_host::time::Instant;
#[cfg(not(feature = "hosted"))]
use std::time::Instant;

use lumen::embed::{Ctx, NativeError, OpError, RealmHandle, Value};
#[cfg(feature = "hosted")]
use lumen_host::{CallbackQueue, Extension};

/// The extension a runtime installs: the five timer globals plus the [`Timers`] state.
#[cfg(feature = "hosted")]
pub fn extension() -> Extension {
    Extension {
        name: "timers",
        modules: &[
            lumen_host::globals::<globals::Module>,
            lumen_host::globals::<immediate::Module>,
        ],
        state_init: Some(|state| state.put(Timers::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}

/// Install only the timer globals, without the hosted event-loop substrate (no `setImmediate`).
/// At most `max_timers` timers may be pending at once.
pub fn install(engine: &mut lumen::Engine, max_timers: usize) {
    engine.ctx().op_state().put(Timers {
        limit: Some(max_timers),
        ..Timers::default()
    });
    if engine.define_globals::<globals::Module>().is_err() {
        panic!("timer globals install");
    }
}

struct Entry {
    callback: Value,
    args: Vec<Value>,
    /// The global whose timer API created this entry. Retaining the handle makes realm identity
    /// stable while the timer is live and lets navigation drop every callback owned by a page.
    owner: RealmHandle,
    delay: Duration,
    /// `Some(period)` for `setInterval`: reschedule after each firing.
    repeat: Option<Duration>,
    /// The deadline the live heap node carries; a node with any other deadline is stale
    /// (the timer was refreshed) and is skipped like a cancelled one.
    deadline: Instant,
    /// Node's `timer.unref()`: an unref'd timer still fires if the loop is alive for another
    /// reason, but does not by itself keep it alive.
    refed: bool,
}

/// The timer heap. Cancellation is lazy: `clear*` removes the entry; stale heap nodes are
/// skipped (and popped) when they surface, so `clearTimeout` is O(1).
#[derive(Default)]
pub struct Timers {
    limit: Option<usize>,
    next_id: u64,
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    entries: HashMap<u64, Entry>,
}

impl Timers {
    fn schedule(
        &mut self,
        callback: Value,
        args: Vec<Value>,
        owner: RealmHandle,
        delay: Duration,
        repeat: bool,
        deadline: Instant,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.insert(
            id,
            Entry {
                callback,
                args,
                owner,
                delay,
                repeat: repeat.then_some(delay),
                deadline,
                refed: true,
            },
        );
        self.heap.push(Reverse((deadline, id)));
        id
    }

    fn clear(&mut self, id: u64, owner: &RealmHandle) {
        if self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.owner.same_realm(owner))
        {
            self.entries.remove(&id);
        }
        self.compact_stale_heap();
    }

    /// Drop every pending timer and interval owned by `realm`, returning the number cancelled.
    /// This is the host navigation/discard path; it also drops the callbacks and their captured
    /// values immediately. Other realms' timers remain in the shared heap.
    pub fn cancel_realm(&mut self, realm: &RealmHandle) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| !entry.owner.same_realm(realm));
        let cancelled = before - self.entries.len();
        if cancelled != 0 {
            self.compact_stale_heap();
        }
        cancelled
    }

    /// Number of pending timer entries owned by `realm`.
    pub fn pending_for_realm(&self, realm: &RealmHandle) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.owner.same_realm(realm))
            .count()
    }

    fn compact_stale_heap(&mut self) {
        // Lazy cancellation leaves stale heap nodes; bound them so clear-heavy scripts
        // cannot grow the heap without limit.
        if self.heap.len() > self.entries.len().saturating_mul(2) + 32 {
            let entries = &self.entries;
            self.heap.retain(|Reverse((deadline, id))| {
                entries
                    .get(id)
                    .is_some_and(|entry| entry.deadline == *deadline)
            });
        }
    }

    /// `timer.ref()` / `timer.unref()`; false when the timer no longer exists.
    pub fn set_ref(&mut self, id: u64, owner: &RealmHandle, refed: bool) -> bool {
        match self.entries.get_mut(&id) {
            Some(e) if e.owner.same_realm(owner) => {
                e.refed = refed;
                true
            }
            Some(_) | None => false,
        }
    }

    /// `timer.refresh()`: restart a live timer's delay from now. False when it has already
    /// fired (a one-shot) or was cleared — the caller re-schedules it then.
    pub fn refresh(&mut self, id: u64, owner: &RealmHandle) -> bool {
        let Some(e) = self
            .entries
            .get_mut(&id)
            .filter(|entry| entry.owner.same_realm(owner))
        else {
            return false;
        };
        e.deadline = Instant::now() + e.delay;
        self.heap.push(Reverse((e.deadline, id)));
        true
    }

    /// Whether any live ref'd timer remains (the loop stays alive while true).
    pub fn has_pending(&self) -> bool {
        self.entries.values().any(|e| e.refed)
    }

    fn is_live(&self, id: u64, deadline: Instant) -> bool {
        self.entries
            .get(&id)
            .is_some_and(|e| e.deadline == deadline)
    }

    /// When the loop may sleep until. Pops cancelled and stale heap nodes so a cleared or
    /// refreshed timer can't produce a busy-wakeup loop.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if self.is_live(id, deadline) {
                return Some(deadline);
            }
            self.heap.pop();
        }
        None
    }

    /// Callbacks due at `now`, earliest first. Intervals are rescheduled (from their
    /// deadline, not `now`, so periods don't drift); one-shots are removed.
    ///
    /// A batch is a snapshot: a callback that clears or refreshes a later timer in the same batch
    /// does not stop it firing. The event loop uses [`Timers::take_next_due`] instead.
    pub fn take_due(&mut self, now: Instant) -> Vec<(Value, Vec<Value>)> {
        let mut due = Vec::new();
        while let Some(next) = self.take_next_due(now) {
            due.push(next);
        }
        due
    }

    /// The earliest callback due at `now`, if any, taken the same way as [`Timers::take_due`].
    /// Taking one at a time and running it before taking the next is what lets a timer callback
    /// cancel or refresh another timer that is due in the same turn, as in Node.
    pub fn take_next_due(&mut self, now: Instant) -> Option<(Value, Vec<Value>)> {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if deadline > now {
                return None;
            }
            self.heap.pop();
            if !self.is_live(id, deadline) {
                continue; // cancelled, or superseded by a refresh
            }
            let entry = self.entries.get_mut(&id).expect("live entry");
            let due = (entry.callback.clone(), entry.args.clone());
            match entry.repeat {
                Some(period) => {
                    // Keep the cadence, catching up on a short lag (the OS timer granularity makes
                    // a 1 ms interval wake late), but never accumulate an unbounded backlog: an
                    // interval whose callback runs longer than its period would otherwise stay
                    // due forever and starve I/O completions. Past `MAX_INTERVAL_LAG` it re-arms
                    // from the current loop time, as Node does. A zero period
                    // (`setInterval(f, 0)`) is re-armed 1 ms out, so a pass terminates.
                    const MAX_INTERVAL_LAG: Duration = Duration::from_millis(50);
                    let period = period.max(Duration::from_millis(1));
                    let next = deadline + period;
                    entry.deadline = if now.saturating_duration_since(next) > MAX_INTERVAL_LAG {
                        now + period
                    } else {
                        next
                    };
                    self.heap.push(Reverse((entry.deadline, id)));
                }
                None => {
                    self.entries.remove(&id);
                }
            }
            return Some(due);
        }
        None
    }
}

/// WHATWG timer-initialization steps, abridged: coerce the delay (NaN/negative -> 0), stash
/// callback + extra args, return the id as a Number.
fn schedule(
    ctx: &mut Ctx,
    callback: Value,
    delay: Value,
    args: &[Value],
    repeat: bool,
) -> Result<f64, OpError> {
    if !callback.is_callable() {
        let kind = if repeat { "setInterval" } else { "setTimeout" };
        return Err(NativeError::type_error(format!("{kind} expects a function")).into());
    }
    let ms = ctx.coerce_number(&delay)?;
    let delay = Duration::from_millis(if ms.is_finite() && ms > 0.0 {
        ms as u64
    } else {
        0
    });
    let deadline = Instant::now()
        .checked_add(delay)
        .ok_or_else(|| NativeError::overflow("timer delay exceeds the clock range"))?;
    let owner = ctx.current_host_realm();
    let timers = ctx.host_mut::<Timers>().expect("timers state installed");
    if timers
        .limit
        .is_some_and(|limit| timers.entries.len() >= limit)
    {
        return Err(NativeError::overflow("timer capacity exhausted").into());
    }
    Ok(timers.schedule(callback, args.to_vec(), owner, delay, repeat, deadline) as f64)
}

/// A timer id argument; `None` for ids that name no timer (non-numeric, negative, NaN).
fn timer_id(ctx: &mut Ctx, id: &Value) -> Result<Option<u64>, Value> {
    if matches!(id, Value::Undefined) {
        return Ok(None);
    }
    let id = ctx.coerce_number(id)?;
    Ok((id.is_finite() && id >= 0.0).then_some(id as u64))
}

/// Shared by `clearTimeout`/`clearInterval` (per spec either clears either kind). Unknown or
/// non-numeric ids are ignored.
fn clear_timer(ctx: &mut Ctx, id: &Value) -> Result<(), Value> {
    if let Some(id) = timer_id(ctx, id)? {
        let owner = ctx.current_host_realm();
        ctx.host_mut::<Timers>()
            .expect("timers state installed")
            .clear(id, &owner);
    }
    Ok(())
}

/// The timer globals; `__timerSetRef` / `__timerRefresh` are Node's Timeout handle methods,
/// wrapped by the node:timers glue.
#[lumen_bind::module(name = "timers")]
mod globals {
    use super::*;

    #[op(name = "setTimeout")]
    fn set_timeout(
        ctx: &mut Ctx,
        callback: Value,
        delay: Value,
        #[varargs] args: &[Value],
    ) -> Result<f64, OpError> {
        schedule(ctx, callback, delay, args, false)
    }

    #[op(name = "setInterval")]
    fn set_interval(
        ctx: &mut Ctx,
        callback: Value,
        delay: Value,
        #[varargs] args: &[Value],
    ) -> Result<f64, OpError> {
        schedule(ctx, callback, delay, args, true)
    }

    #[op(name = "clearTimeout")]
    fn clear_timeout(ctx: &mut Ctx, id: Value) -> Result<(), Value> {
        clear_timer(ctx, &id)
    }

    #[op(name = "clearInterval")]
    fn clear_interval(ctx: &mut Ctx, id: Value) -> Result<(), Value> {
        clear_timer(ctx, &id)
    }

    /// `(id, refed)` — Node's `timer.ref()`/`unref()`; returns whether the timer is still live.
    #[op(name = "__timerSetRef")]
    fn timer_set_ref(ctx: &mut Ctx, id: Value, refed: Value) -> Result<bool, Value> {
        let Some(id) = timer_id(ctx, &id)? else {
            return Ok(false);
        };
        let owner = ctx.current_host_realm();
        let refed = !matches!(refed, Value::Bool(false));
        Ok(ctx
            .host_mut::<Timers>()
            .expect("timers state installed")
            .set_ref(id, &owner, refed))
    }

    /// `(id)` — Node's `timer.refresh()`; false when the timer must be re-scheduled from scratch.
    #[op(name = "__timerRefresh")]
    fn timer_refresh(ctx: &mut Ctx, id: Value) -> Result<bool, Value> {
        let Some(id) = timer_id(ctx, &id)? else {
            return Ok(false);
        };
        let owner = ctx.current_host_realm();
        Ok(ctx
            .host_mut::<Timers>()
            .expect("timers state installed")
            .refresh(id, &owner))
    }
}

/// `setImmediate` needs the hosted event loop's [`CallbackQueue`].
#[cfg(feature = "hosted")]
#[lumen_bind::module(name = "timers_immediate")]
mod immediate {
    use super::*;

    /// Queue for the next loop turn (after microtasks, before timers get another look).
    #[op(name = "setImmediate")]
    fn set_immediate(
        ctx: &mut Ctx,
        callback: Value,
        #[varargs] args: &[Value],
    ) -> Result<(), NativeError> {
        if !callback.is_callable() {
            return Err(NativeError::type_error("setImmediate expects a function"));
        }
        CallbackQueue::enqueue(ctx.op_state(), callback, args.to_vec());
        Ok(())
    }
}
