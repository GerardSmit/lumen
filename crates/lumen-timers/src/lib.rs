//! lumen-timers — the timer globals (`setTimeout`, `setInterval`, `clearTimeout`,
//! `clearInterval`, `setImmediate`) as an op crate.
//!
//! The ops only mutate the [`Timers`] heap in `OpState`; nothing here sleeps, spawns, or
//! fires. The runtime's event loop drives everything: it asks [`Timers::next_deadline`] how
//! long it may block, and fires [`Timers::take_due`] callbacks each turn. `setImmediate`
//! doesn't touch the heap at all — it queues on the loop's [`CallbackQueue`] for the next
//! turn.

use std::time::Duration;
use std::rc::Rc;
use std::collections::HashMap;

use lumen_common::deadline::DeadlineQueue;

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
        lazy_globals: &[],
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
    /// Node's `timer.unref()`: an unref'd timer still fires if the loop is alive for another
    /// reason, but does not by itself keep it alive.
    refed: bool,
    browser: Option<Box<BrowserEntry>>,
}

/// Browser adapters supply actual settings/policy and classic-script execution.
/// These function pointers own no realm or JavaScript handles.
pub struct BrowserHooks {
    pub capture: fn(&mut Ctx, &str, bool) -> Result<Rc<lumen::ClassicScriptContext>, OpError>,
    pub eligible: fn(&mut Ctx) -> bool,
    pub script: fn(&mut Ctx, &str, Rc<lumen::ClassicScriptContext>) -> Result<(), Value>,
    pub report: fn(&mut Ctx, Value) -> bool,
}

#[derive(Clone)]
pub struct TimerScript {
    pub source: String,
    pub context: Rc<lumen::ClassicScriptContext>,
}

struct BrowserEntry {
    public_id: i32,
    script: Option<Rc<TimerScript>>,
    hooks: Rc<BrowserHooks>,
    nesting: u32,
    generation: u64,
    timeout: u32,
}

pub struct TimerFiring {
    pub callback: Value,
    pub args: Vec<Value>,
    pub owner: RealmHandle,
    browser: Option<BrowserFiring>,
}

struct BrowserFiring {
    id: u64,
    generation: u64,
    nesting: u32,
    script: Option<Rc<TimerScript>>,
    hooks: Rc<BrowserHooks>,
}

impl TimerFiring {
    pub fn is_browser(&self) -> bool { self.browser.is_some() }
    pub fn script_context(&self) -> Option<&lumen::ClassicScriptContext> {
        self.browser.as_ref()?.script.as_ref().map(|script| script.context.as_ref())
    }
}

/// The timer heap. Cancellation is lazy: `clear*` removes the entry; stale heap nodes are
/// skipped (and popped) when they surface, so `clearTimeout` is O(1).
#[derive(Default)]
pub struct Timers {
    limit: Option<usize>,
    queue: DeadlineQueue<Instant, Entry>,
    unref_count: usize,
    current_nesting: Option<u32>,
    // HTML's ID map is realm-qualified; the deadline heap keeps its own unique
    // handles. No JavaScript handles are retained by these lookup keys.
    browser_ids: HashMap<(usize, i32), u64>,
    next_browser_id: i32,
}

impl Timers {
    fn remove_entry(&mut self, id: u64) {
        if let Some(entry) = self.queue.remove(id) {
            if let Some(browser) = &entry.browser {
                self.browser_ids.remove(&(realm_key(&entry.owner), browser.public_id));
                if self.browser_ids.is_empty() { self.browser_ids = HashMap::new(); }
            }
            if !entry.refed {
                self.unref_count -= 1;
            }
        }
    }

    fn schedule(
        &mut self,
        callback: Value,
        args: Vec<Value>,
        owner: RealmHandle,
        delay: Duration,
        repeat: bool,
        deadline: Instant,
    ) -> u64 {
        self.queue.insert(
            deadline,
            Entry {
                callback,
                args,
                owner,
                delay,
                repeat: repeat.then_some(delay),
                refed: true,
                browser: None,
            },
        )
    }

    fn clear(&mut self, id: u64, owner: &RealmHandle) {
        if self
            .queue
            .get(id)
            .is_some_and(|entry| entry.owner.same_realm(owner))
        {
            self.remove_entry(id);
        }
        self.queue.compact();
        if self.queue.is_empty() {
            self.queue.release_idle_capacity();
        }
    }

    /// Drop every pending timer and interval owned by `realm`, returning the number cancelled.
    /// This is the host navigation/discard path; it also drops the callbacks and their captured
    /// values immediately. Other realms' timers remain in the shared heap.
    pub fn cancel_realm(&mut self, realm: &RealmHandle) -> usize {
        let before = self.queue.len();
        let mut unref_removed = 0;
        self.queue.retain(|_, entry| {
            let keep = !entry.owner.same_realm(realm);
            if !keep && !entry.refed {
                unref_removed += 1;
            }
            keep
        });
        self.unref_count -= unref_removed;
        let key = realm_key(realm);
        self.browser_ids.retain(|(owner, _), _| *owner != key);
        if self.browser_ids.is_empty() { self.browser_ids = HashMap::new(); }
        let cancelled = before - self.queue.len();
        if cancelled != 0 {
            self.queue.compact();
            self.queue.release_idle_capacity();
        }
        cancelled
    }

    /// Number of pending timer entries owned by `realm`.
    pub fn pending_for_realm(&self, realm: &RealmHandle) -> usize {
        self.queue
            .values()
            .filter(|entry| entry.owner.same_realm(realm))
            .count()
    }

    /// `timer.ref()` / `timer.unref()`; false when the timer no longer exists.
    pub fn set_ref(&mut self, id: u64, owner: &RealmHandle, refed: bool) -> bool {
        match self.queue.get_mut(id) {
            Some(e) if e.owner.same_realm(owner) => {
                if e.refed != refed {
                    if refed { self.unref_count -= 1; } else { self.unref_count += 1; }
                }
                e.refed = refed;
                true
            }
            Some(_) | None => false,
        }
    }

    /// `timer.refresh()`: restart a live timer's delay from now. False when it has already
    /// fired (a one-shot) or was cleared — the caller re-schedules it then.
    pub fn refresh(&mut self, id: u64, owner: &RealmHandle) -> bool {
        let Some(delay) = self
            .queue
            .get(id)
            .filter(|entry| entry.owner.same_realm(owner))
            .map(|entry| entry.delay)
        else {
            return false;
        };
        self.queue.rearm(id, Instant::now() + delay)
    }

    /// Whether any live ref'd timer remains (the loop stays alive while true).
    pub fn has_pending(&self) -> bool {
        self.queue.len() > self.unref_count
    }

    /// When the loop may sleep until. Pops cancelled and stale heap nodes so a cleared or
    /// refreshed timer can't produce a busy-wakeup loop.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        self.queue.next_deadline()
    }

    /// Callbacks due at `now`, earliest first. Intervals are rescheduled (from their
    /// deadline, not `now`, so periods don't drift); one-shots are removed.
    ///
    /// A batch is a snapshot: a callback that clears or refreshes a later timer in the same batch
    /// does not stop it firing. The event loop uses [`Timers::take_next_due`] instead.
    pub fn take_due(&mut self, now: Instant) -> Vec<(Value, Vec<Value>)> {
        let mut due = Vec::new();
        while let Some((callback, args, _owner)) = self.take_next_due(now) {
            due.push((callback, args));
        }
        due
    }

    /// The earliest callback due at `now`, if any, taken the same way as [`Timers::take_due`].
    /// Taking one at a time and running it before taking the next is what lets a timer callback
    /// cancel or refresh another timer that is due in the same turn, as in Node.
    pub fn take_next_due(&mut self, now: Instant) -> Option<(Value, Vec<Value>, RealmHandle)> {
        let firing = self.take_next_firing(now)?;
        // Legacy embeddings only install the callable timer globals. Browser
        // adapters must run the firing ticket and complete its lifecycle.
        assert!(!firing.is_browser(), "browser timer tickets require run_browser_firing");
        Some((firing.callback, firing.args, firing.owner))
    }

    pub fn take_next_firing(&mut self, now: Instant) -> Option<TimerFiring> {
        let (id, deadline) = self.queue.pop_due(now)?;
        let entry = self.queue.get(id).expect("live entry");
        let due = TimerFiring {
            callback: entry.callback.clone(), args: entry.args.clone(), owner: entry.owner.clone(),
            browser: entry.browser.as_ref().map(|browser| BrowserFiring {
                id, generation: browser.generation, nesting: browser.nesting,
                script: browser.script.clone(), hooks: browser.hooks.clone(),
            }),
        };
        if due.is_browser() { return Some(due); }
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
                let next = if now.saturating_duration_since(next) > MAX_INTERVAL_LAG {
                    now + period
                } else {
                    next
                };
                self.queue.rearm(id, next);
            }
            None => {
                self.remove_entry(id);
                if self.queue.is_empty() {
                    self.queue.release_idle_capacity();
                }
            }
        }
        Some(due)
    }

    fn finish_browser_firing(&mut self, firing: &TimerFiring, now: Instant) {
        let Some(ticket) = &firing.browser else { return; };
        let Some(entry) = self.queue.get_mut(ticket.id) else { return; };
        let Some(browser) = entry.browser.as_mut() else { return; };
        if !entry.owner.same_realm(&firing.owner) || browser.generation != ticket.generation { return; }
        if entry.repeat.is_none() {
            self.remove_entry(ticket.id);
        } else {
            browser.nesting = ticket.nesting.saturating_add(1);
            browser.generation = browser.generation.wrapping_add(1);
            let delay = browser_delay(browser.timeout, ticket.nesting);
            entry.delay = delay;
            self.queue.rearm(ticket.id, now + delay);
        }
        self.queue.compact();
        self.queue.release_idle_capacity();
    }
}

fn realm_key(owner: &RealmHandle) -> usize {
    owner.global().object_identity().expect("realm global identity")
}

fn browser_delay(timeout: u32, nesting: u32) -> Duration {
    Duration::from_millis(if nesting > 5 { timeout.max(4) } else { timeout } as u64)
}

/// Execute one browser ticket and finish its timer task before a microtask
/// checkpoint. Failure provenance follows the Function callback or script global.
pub fn run_browser_firing(ctx: &mut Ctx, firing: TimerFiring) -> Option<(Value, RealmHandle)> {
    let ticket = firing.browser.as_ref()?;
    let valid = ctx.host_mut::<Timers>().and_then(|timers| timers.queue.get(ticket.id))
        .is_some_and(|entry| entry.owner.same_realm(&firing.owner)
            && entry.browser.as_ref().is_some_and(|entry| entry.generation == ticket.generation));
    if !valid { return None; }
    let old_nesting = ctx.host_mut::<Timers>().expect("timer heap").current_nesting.replace(ticket.nesting);
    let owner = if ticket.script.is_some() { firing.owner.clone() } else {
        lumen::embed::JsFunction::from_value(firing.callback.clone())
            .and_then(|function| ctx.function_host_realm(&function).ok()).unwrap_or_else(|| firing.owner.clone())
    };
    let mut aborted = false;
    let result = ctx.with_host_realm(&firing.owner, |ctx| {
        if !(ticket.hooks.eligible)(ctx) { aborted = true; return Ok(()); }
        match &ticket.script {
            Some(script) => (ticket.hooks.script)(ctx, &script.source, script.context.clone()),
            None => { let this = ctx.global_this(); ctx.invoke(firing.callback.clone(), this, &firing.args)
                .map(|_| ()) },
        }
    });
    let mut failure = match result {
        Ok(Err(error)) => Some((error, owner)),
        Err(error) => Some((ctx.make_error("Error", error.to_string()), firing.owner.clone())),
        Ok(Ok(())) => None,
    };
    if let Some((error, owner)) = &failure {
        let reported = ctx.with_host_realm(owner, |ctx| (ticket.hooks.report)(ctx, error.clone())).unwrap_or(false);
        if reported { failure = None; }
    }
    let timers = ctx.host_mut::<Timers>().expect("timer heap");
    timers.current_nesting = old_nesting;
    if aborted { timers.clear(ticket.id, &firing.owner); }
    else { timers.finish_browser_firing(&firing, Instant::now()); }
    failure
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
    schedule_delay(ctx, callback, args, delay, repeat).map(|id| id as f64)
}

/// Schedule a host-owned one-shot callback in the current realm using the same bounded timer
/// heap and cancellation/lifetime rules as the JavaScript timer globals. Browser subsystems use
/// this for asynchronous operations whose completion must be ordered by real elapsed time, rather
/// than introducing a second callback queue or blocking the event loop.
/// Cancel only a timer owned by the current host realm. Native browser services
/// use this same heap admission/cancellation path as author timers.
pub fn cancel_host_callback(ctx:&mut Ctx,id:u64) {
    let owner=ctx.current_host_realm();
    if let Some(timers)=ctx.host_mut::<Timers>() {timers.clear(id,&owner);}
}

pub fn schedule_host_callback(
    ctx: &mut Ctx,
    callback: Value,
    args: &[Value],
    delay: Duration,
) -> Result<u64, OpError> {
    if !callback.is_callable() {
        return Err(NativeError::type_error("host timer callback must be callable").into());
    }
    schedule_delay(ctx, callback, args, delay, false)
}

fn schedule_delay(
    ctx: &mut Ctx,
    callback: Value,
    args: &[Value],
    delay: Duration,
    repeat: bool,
) -> Result<u64, OpError> {
    let deadline = Instant::now()
        .checked_add(delay)
        .ok_or_else(|| NativeError::overflow("timer delay exceeds the clock range"))?;
    let owner = ctx.current_host_realm();
    let timers = ctx
        .host_mut::<Timers>()
        .ok_or_else(|| NativeError::type_error("timer host state is not installed"))?;
    if timers
        .limit
        .is_some_and(|limit| timers.queue.len() >= limit)
    {
        return Err(NativeError::overflow("timer capacity exhausted").into());
    }
    Ok(timers.schedule(callback, args.to_vec(), owner, delay, repeat, deadline))
}

#[cfg(feature = "hosted")]
pub fn install_browser(ctx: &mut Ctx, hooks: BrowserHooks) -> Result<(), OpError> {
    if ctx.host_mut::<Timers>().is_none() { ctx.op_state().put(Timers::default()); }
    lumen_host::realm_services::RealmServices::replace_current(ctx, hooks);
    let global = ctx.global_object();
    ctx.install_module::<browser::Module>(&global).map_err(OpError::thrown)
}

#[cfg(feature = "hosted")]
fn schedule_browser(ctx: &mut Ctx, handler: Value, timeout: Value, args: &[Value], repeat: bool) -> Result<f64, OpError> {
    let hooks = lumen_host::realm_services::RealmServices::<BrowserHooks>::current(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "browser timer settings unavailable"))?;
    // WebIDL union conversion precedes timeout conversion and ID allocation.
    let (callback, source) = if handler.is_callable() { (handler, None) } else {
        let source = ctx.coerce_string(&handler)?.to_string();
        (Value::Undefined, Some(source))
    };
    let timeout = ctx.webidl_long(&timeout)?;
    let timeout = timeout.max(0) as u32;
    // Trusted Types is timer initialization step 1, after all WebIDL argument
    // conversions; a timeout conversion may itself execute author code.
    let script = match source {
        Some(source) => { let context = (hooks.capture)(ctx, &source, repeat)?;
            Some(Rc::new(TimerScript { source, context })) },
        None => None,
    };
    let owner = ctx.current_host_realm();
    let timers = ctx.host_mut::<Timers>().ok_or_else(|| OpError::new("InvalidStateError", "timer heap unavailable"))?;
    if timers.limit.is_some_and(|limit| timers.queue.len() >= limit) { return Err(NativeError::overflow("timer capacity exhausted").into()); }
    let nesting = timers.current_nesting.unwrap_or(0);
    let delay = browser_delay(timeout, nesting);
    let deadline = Instant::now().checked_add(delay).ok_or_else(|| NativeError::overflow("timer delay exceeds clock range"))?;
    let key = realm_key(&owner);
    // One shared cursor gives the ordinary allocation O(1), including pages
    // with many timers. Wrap within positive WebIDL long and skip live IDs in
    // this realm; the heap handle and firing generation never alias reused IDs.
    let mut public_id = timers.next_browser_id.checked_add(1).unwrap_or(1);
    let first = public_id;
    while timers.browser_ids.contains_key(&(key, public_id)) {
        public_id = public_id.checked_add(1).unwrap_or(1);
        if public_id == first { return Err(NativeError::overflow("browser timer IDs exhausted").into()); }
    }
    timers.next_browser_id = public_id;
    // The string-script branch never consumes extra arguments. Retaining them
    // would unnecessarily keep arbitrary author objects alive until firing.
    let arguments = if script.is_some() { Vec::new() } else { args.to_vec() };
    let id = timers.schedule(callback, arguments, owner, delay, repeat, deadline);
    timers.browser_ids.insert((key, public_id), id);
    timers.queue.get_mut(id).expect("new timer").browser = Some(Box::new(BrowserEntry {
        public_id, script, hooks, nesting: nesting.saturating_add(1), generation: 1, timeout,
    }));
    Ok(public_id as f64)
}

#[cfg(feature = "hosted")]
#[lumen_bind::module(name = "browser_timers")]
mod browser {
    use super::*;
    #[op(name = "setTimeout")]
    fn set_timeout(ctx: &mut Ctx, handler: Value, #[default(Value::Num(0.0))] timeout: Value, #[varargs] args: &[Value]) -> Result<f64, OpError> {
        schedule_browser(ctx, handler, timeout, args, false)
    }
    #[op(name = "setInterval")]
    fn set_interval(ctx: &mut Ctx, handler: Value, #[default(Value::Num(0.0))] timeout: Value, #[varargs] args: &[Value]) -> Result<f64, OpError> {
        schedule_browser(ctx, handler, timeout, args, true)
    }
    #[op(name = "clearTimeout")]
    fn clear_timeout(ctx: &mut Ctx, #[default(Value::Num(0.0))] id: Value) -> Result<(), Value> { clear_browser_timer(ctx, id) }
    #[op(name = "clearInterval")]
    fn clear_interval(ctx: &mut Ctx, #[default(Value::Num(0.0))] id: Value) -> Result<(), Value> { clear_browser_timer(ctx, id) }
    fn clear_browser_timer(ctx: &mut Ctx, id: Value) -> Result<(), Value> {
        let id = ctx.webidl_long(&id)?;
        let owner = ctx.current_host_realm();
        if id > 0 {
            let timers = ctx.host_mut::<Timers>().expect("timer heap");
            if let Some(handle) = timers.browser_ids.get(&(realm_key(&owner), id)).copied() {
                timers.clear(handle, &owner);
            }
        }
        Ok(())
    }
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
        #[default(Value::Num(0.0))] delay: Value,
        #[varargs] args: &[Value],
    ) -> Result<f64, OpError> {
        schedule(ctx, callback, delay, args, false)
    }

    #[op(name = "setInterval")]
    fn set_interval(
        ctx: &mut Ctx,
        callback: Value,
        #[default(Value::Num(0.0))] delay: Value,
        #[varargs] args: &[Value],
    ) -> Result<f64, OpError> {
        schedule(ctx, callback, delay, args, true)
    }

    #[op(name = "clearTimeout")]
    fn clear_timeout(ctx: &mut Ctx, #[default(Value::Num(0.0))] id: Value) -> Result<(), Value> {
        clear_timer(ctx, &id)
    }

    #[op(name = "clearInterval")]
    fn clear_interval(ctx: &mut Ctx, #[default(Value::Num(0.0))] id: Value) -> Result<(), Value> {
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

#[cfg(all(test, feature = "hosted"))]
mod tests {
    use super::*;

    fn hooks() -> BrowserHooks {
        BrowserHooks {
            capture: |ctx, _, _| Ok(ctx.default_classic_script_context()), eligible: |_| true,
            script: |ctx, source, context| ctx.run_classic_script(source, context).map(|_| ()).map_err(lumen::embed::abrupt_value),
            report: |_, _| false,
        }
    }

    #[test]
    fn specification_browser_timer_generations_repeat_after_execution_and_reset_nesting() {
        let mut engine = lumen::Engine::new();
        install_browser(engine.ctx(), hooks()).unwrap_or_else(|_| panic!("browser timers install"));
        engine.eval_value("globalThis.calls=0; globalThis.repeat=setInterval(()=>{calls++;},0)")
            .expect("interval parses").ok().expect("interval registers");
        let now = Instant::now() + Duration::from_secs(1);
        let first = engine.ctx().host_mut::<Timers>().expect("heap").take_next_firing(now).expect("first ticket");
        let duplicate = TimerFiring { callback: first.callback.clone(), args: first.args.clone(), owner: first.owner.clone(),
            browser: first.browser.as_ref().map(|ticket| BrowserFiring { id: ticket.id, generation: ticket.generation,
                nesting: ticket.nesting, script: ticket.script.clone(), hooks: ticket.hooks.clone() }) };
        let id = first.browser.as_ref().expect("browser ticket").id;
        assert_eq!(engine.ctx().host_mut::<Timers>().expect("heap").queue.get(id).expect("entry").browser.as_ref().expect("browser").generation, 1);
        assert!(run_browser_firing(engine.ctx(), first).is_none());
        assert_eq!(engine.ctx().host_mut::<Timers>().expect("heap").queue.get(id).expect("entry").browser.as_ref().expect("browser").generation, 2);
        assert!(run_browser_firing(engine.ctx(), duplicate).is_none());
        assert!(matches!(engine.eval_value("calls"), Ok(Ok(Value::Num(1.0)))), "stale generation cannot execute or repeat");
        let next = engine.ctx().host_mut::<Timers>().expect("heap").take_next_firing(now).expect("repeat ticket");
        engine.eval_value("clearTimeout(repeat)").expect("clear parses").ok().expect("clear runs");
        assert!(run_browser_firing(engine.ctx(), next).is_none());
        assert!(matches!(engine.eval_value("calls"), Ok(Ok(Value::Num(1.0)))), "clearing the ID invalidates an already-ready ticket");
        assert_eq!(browser_delay(0, 5), Duration::ZERO);
        assert_eq!(browser_delay(0, 6), Duration::from_millis(4));
        engine.ctx().host_mut::<Timers>().expect("heap").current_nesting = Some(6);
        engine.eval_value("globalThis.nested=setTimeout(()=>{},0)").expect("nested parses").ok().expect("nested runs");
        engine.ctx().host_mut::<Timers>().expect("heap").current_nesting = None;
        let callback = engine.eval_value("()=>{globalThis.checkpointTimer=setTimeout(()=>{},0)}")
            .expect("microtask parses").ok().expect("microtask evaluates");
        engine.ctx().queue_microtask(callback);
        engine.run_microtasks();
        let timers = engine.ctx().host_mut::<Timers>().expect("heap");
        let mut levels: Vec<_> = timers.queue.values().filter_map(|entry| entry.browser.as_ref().map(|browser| (browser.nesting, entry.delay))).collect();
        levels.sort();
        assert_eq!(levels, [(1, Duration::ZERO), (7, Duration::from_millis(4))], "checkpoint timers do not inherit the preceding task nesting");
    }

    #[test]
    fn specification_browser_timer_public_ids_wrap_without_aliasing_ready_handles() {
        let mut engine = lumen::Engine::new();
        install_browser(engine.ctx(), hooks()).unwrap_or_else(|_| panic!("browser timers install"));
        engine.eval_value("globalThis.oldCalls=0;globalThis.newCalls=0;globalThis.oldId=setTimeout(()=>{oldCalls++},0)")
            .expect("old timer parses").ok().expect("old timer registers");
        let old = engine.ctx().host_mut::<Timers>().expect("heap").take_next_firing(Instant::now() + Duration::from_secs(1)).expect("old ready ticket");
        engine.eval_value("clearInterval(oldId)").expect("clear parses").ok().expect("clear runs");
        engine.ctx().host_mut::<Timers>().expect("heap").next_browser_id = i32::MAX;
        engine.eval_value("globalThis.newId=setTimeout(()=>{newCalls++},0)").expect("new timer parses").ok().expect("new timer registers");
        assert!(matches!(engine.eval_value("oldId === newId && newId === 1"), Ok(Ok(Value::Bool(true)))), "public long wraps and may reuse a cleared ID");
        assert!(run_browser_firing(engine.ctx(), old).is_none());
        let new = engine.ctx().host_mut::<Timers>().expect("heap").take_next_firing(Instant::now() + Duration::from_secs(1)).expect("new ready ticket");
        assert!(run_browser_firing(engine.ctx(), new).is_none());
        assert!(matches!(engine.eval_value("oldCalls === 0 && newCalls === 1"), Ok(Ok(Value::Bool(true)))), "public ID reuse cannot revalidate an old unique handle");
        let timers = engine.ctx().host_mut::<Timers>().expect("heap");
        assert!(timers.browser_ids.is_empty() && timers.queue.is_empty());
    }

    #[test]
    fn specification_browser_timer_cancellation_releases_actual_function_and_source_owners() {
        let mut engine = lumen::Engine::new();
        let child = engine.ctx().create_host_realm();
        let weak = engine.ctx().weak_value(&child.global()).expect("child global");
        let (ignored_argument, function_argument) = engine.ctx().with_host_realm(&child, |ctx| {
            install_browser(ctx, hooks()).unwrap_or_else(|_| panic!("browser timers install"));
            let context = Rc::new(lumen::ClassicScriptContext { base_url: "https://timer.test/captured.js".into(), ..Default::default() });
            let ignored = ctx.run_classic_script("({ marker: 'unused string argument' })", context.clone()).ok().expect("ignored object evaluates");
            let passed = ctx.run_classic_script("({ marker: 'function argument' })", context.clone()).ok().expect("function argument evaluates");
            let ignored_argument = ctx.weak_value(&ignored).expect("ignored object weak identity");
            let function_argument = ctx.weak_value(&passed).expect("function object weak identity");
            let callback = ctx.run_classic_script("(function(extra) {})", context).ok().expect("callback evaluates");
            schedule_browser(ctx, callback, Value::Num(1000.0), &[passed], false)
                .unwrap_or_else(|_| panic!("Function timer registers"));
            schedule_browser(ctx, Value::str("globalThis.pending=true"), Value::Num(1000.0), &[ignored], false)
                .unwrap_or_else(|_| panic!("string timer registers"));
            (ignored_argument, function_argument)
        }).expect("child realm enters");
        engine.ctx().dispose_host_realm(&child).expect("child retires");
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(), "pending timers legitimately retain their admitting global and callback");
        assert!(ignored_argument.upgrade().is_none(), "unused string-timer arguments cannot root objects while the timer remains pending");
        assert!(function_argument.upgrade().is_some(), "Function-timer extra arguments retain their exact object until firing or cancellation");
        assert_eq!(engine.ctx().host_mut::<Timers>().expect("heap").cancel_realm(&child), 2);
        drop(child);
        engine.collect_garbage();
        assert!(function_argument.upgrade().is_none(), "canceling the Function timer releases its actual argument owner");
        assert!(weak.upgrade().is_none(), "source metadata and weak hook registration cannot pin a canceled realm");
    }
}
