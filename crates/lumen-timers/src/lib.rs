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
use std::time::{Duration, Instant};

use lumen_host::{ops, CallbackQueue, Ctx, Extension, Value};

/// The extension a runtime installs: the five timer globals plus the [`Timers`] state.
pub fn extension() -> Extension {
    Extension {
        name: "timers",
        globals: ops![
            "setTimeout" (2) => op_set_timeout,
            "setInterval" (2) => op_set_interval,
            "clearTimeout" (1) => op_clear_timer,
            "clearInterval" (1) => op_clear_timer,
            "setImmediate" (1) => op_set_immediate,
            // Node's Timeout handle methods, wrapped by the node:timers glue.
            "__timerSetRef" (2) => op_timer_set_ref,
            "__timerRefresh" (1) => op_timer_refresh,
        ],
        namespaces: &[],
        state_init: Some(|state| state.put(Timers::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}

struct Entry {
    callback: Value,
    args: Vec<Value>,
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
    next_id: u64,
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    entries: HashMap<u64, Entry>,
}

impl Timers {
    fn schedule(
        &mut self,
        callback: Value,
        args: Vec<Value>,
        delay: Duration,
        repeat: bool,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let deadline = Instant::now() + delay;
        self.entries.insert(
            id,
            Entry {
                callback,
                args,
                delay,
                repeat: repeat.then_some(delay),
                deadline,
                refed: true,
            },
        );
        self.heap.push(Reverse((deadline, id)));
        id
    }

    fn clear(&mut self, id: u64) {
        self.entries.remove(&id);
    }

    /// `timer.ref()` / `timer.unref()`; false when the timer no longer exists.
    pub fn set_ref(&mut self, id: u64, refed: bool) -> bool {
        match self.entries.get_mut(&id) {
            Some(e) => {
                e.refed = refed;
                true
            }
            None => false,
        }
    }

    /// `timer.refresh()`: restart a live timer's delay from now. False when it has already
    /// fired (a one-shot) or was cleared — the caller re-schedules it then.
    pub fn refresh(&mut self, id: u64) -> bool {
        let Some(e) = self.entries.get_mut(&id) else {
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
        self.entries.get(&id).is_some_and(|e| e.deadline == deadline)
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
    pub fn take_due(&mut self, now: Instant) -> Vec<(Value, Vec<Value>)> {
        let mut due = Vec::new();
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if deadline > now {
                break;
            }
            self.heap.pop();
            if !self.is_live(id, deadline) {
                continue; // cancelled, or superseded by a refresh
            }
            let entry = self.entries.get_mut(&id).expect("live entry");
            due.push((entry.callback.clone(), entry.args.clone()));
            match entry.repeat {
                Some(period) => {
                    entry.deadline = deadline + period;
                    self.heap.push(Reverse((entry.deadline, id)));
                }
                None => {
                    self.entries.remove(&id);
                }
            }
        }
        due
    }
}

/// WHATWG timer-initialization steps, abridged: coerce the delay (NaN/negative -> 0), stash
/// callback + extra args, return the id as a Number.
fn schedule_op(ctx: &mut Ctx, args: &[Value], repeat: bool) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => {
            let kind = if repeat { "setInterval" } else { "setTimeout" };
            return Err(ctx.make_error("TypeError", format!("{kind} expects a function")));
        }
    };
    let ms = match args.get(1) {
        Some(v) => ctx.coerce_number(v)?,
        None => 0.0,
    };
    let delay = Duration::from_millis(if ms.is_finite() && ms > 0.0 {
        ms as u64
    } else {
        0
    });
    let extra: Vec<Value> = args.iter().skip(2).cloned().collect();
    let timers = ctx.host_mut::<Timers>().expect("timers state installed");
    let id = timers.schedule(callback, extra, delay, repeat);
    Ok(Value::Num(id as f64))
}

fn op_set_timeout(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    schedule_op(ctx, args, false)
}

fn op_set_interval(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    schedule_op(ctx, args, true)
}

/// Shared by `clearTimeout`/`clearInterval` (per spec either clears either kind). Unknown or
/// non-numeric ids are ignored.
fn op_clear_timer(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if let Some(v) = args.first() {
        let id = ctx.coerce_number(v)?;
        if id.is_finite() && id >= 0.0 {
            let timers = ctx.host_mut::<Timers>().expect("timers state installed");
            timers.clear(id as u64);
        }
    }
    Ok(Value::Undefined)
}

fn timer_id_arg(ctx: &mut Ctx, args: &[Value]) -> Result<Option<u64>, Value> {
    match args.first() {
        Some(v) => {
            let id = ctx.coerce_number(v)?;
            Ok((id.is_finite() && id >= 0.0).then_some(id as u64))
        }
        None => Ok(None),
    }
}

/// `(id, refed)` — Node's `timer.ref()`/`unref()`; returns whether the timer is still live.
fn op_timer_set_ref(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(id) = timer_id_arg(ctx, args)? else {
        return Ok(Value::Bool(false));
    };
    let refed = !matches!(args.get(1), Some(Value::Bool(false)));
    let timers = ctx.host_mut::<Timers>().expect("timers state installed");
    Ok(Value::Bool(timers.set_ref(id, refed)))
}

/// `(id)` — Node's `timer.refresh()`; false when the timer must be re-scheduled from scratch.
fn op_timer_refresh(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(id) = timer_id_arg(ctx, args)? else {
        return Ok(Value::Bool(false));
    };
    let timers = ctx.host_mut::<Timers>().expect("timers state installed");
    Ok(Value::Bool(timers.refresh(id)))
}

/// Queue for the next loop turn (after microtasks, before timers get another look).
fn op_set_immediate(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => return Err(ctx.make_error("TypeError", "setImmediate expects a function")),
    };
    let extra: Vec<Value> = args.iter().skip(1).cloned().collect();
    CallbackQueue::enqueue(ctx.op_state(), callback, extra);
    Ok(Value::Undefined)
}
