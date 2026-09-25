//! Promise fast paths: promise state stored in the promise object itself, cached intrinsics and
//! resolving-function parts, and allocation-free `await` reactions.
//!
//! **Inline state.** A promise's [[PromiseState]], [[PromiseResult]], reactions and
//! rejection-tracking bit live in a [`PromiseSlot`] boxed into the object's `call` field
//! ([`Callable::Promise`], which is not callable). Every `then`/resolve reads and writes the
//! object directly - no pointer-keyed side table, no weak pins, no pruning. The collector traces a
//! slot's value and reactions as ordinary heap edges (see `gc_edges`). A suspended async
//! function's coroutine lives in its result promise's slot too, so resuming it is a field read.
//!
//! **Await reactions.** An `await` subscribes the suspended async function to the awaited
//! promise with a reaction whose handler is the async function's own promise and whose result
//! slot holds [`Value::Empty`] (never a real result, which is a promise or `undefined`). The job
//! runner resumes the coroutine directly: no bound-function pair, no native function objects and
//! no throwaway result promise per `await` (spec: Await performs PerformPromiseThen with no
//! result capability).
use crate::interpreter::{Interp, Job};
use crate::value::{Callable, Gc, Object, Property, Props, Value};
use std::rc::Rc;
use std::cell::{Cell, RefCell};

/// [[PromiseState]] values of [`PromiseSlot::status`].
pub(crate) const PENDING: u8 = 0;
pub(crate) const FULFILLED: u8 = 1;
pub(crate) const REJECTED: u8 = 2;
/// A native `super()` grafted this promise's state onto a subclass instance; `value` is that
/// instance, and the resolving functions still bound to this object settle it instead.
pub(crate) const FORWARDED: u8 = 3;

/// [`Reaction::kind`]: an ordinary `then` reaction (handlers + derived promise).
pub(crate) const REACT_THEN: u8 = 0;
/// A `Promise.all` element on a native promise: fulfilment records element `idx` of the
/// combinator state on `result` (the combinator's promise); rejection rejects `result`.
pub(crate) const REACT_ALL: u8 = 1;
/// A `Promise.allSettled` element: either settlement records element `idx`.
pub(crate) const REACT_ALL_SETTLED: u8 = 2;
/// A `Promise.any` element: rejection records element `idx`; fulfilment resolves `result`.
pub(crate) const REACT_ANY: u8 = 3;
/// A `Promise.race` element: either settlement settles `result`.
pub(crate) const REACT_RACE: u8 = 4;

/// [`Job::kind`]: run a reaction handler (or pass the settlement through, or resume an await).
pub(crate) const JOB_REACTION: u8 = 0;
/// PromiseResolveThenableJob: `handler` is the `then` function, `value` the thenable,
/// `result` the promise being resolved.
pub(crate) const JOB_THENABLE: u8 = 1;
/// `queueMicrotask(cb)`: call `handler` with no arguments; a throw becomes an unhandled
/// rejection, as it did when the host shim was `Promise.resolve().then(cb)`.
pub(crate) const JOB_TASK: u8 = 255;
/// Combinator element settlements are `JOB_COMB_BASE + REACT_*` (see
/// [`Interp::combinator_settle`]).
pub(crate) const JOB_COMB_BASE: u8 = 2;

/// One PromiseReaction pair (fulfil and reject handlers share the result and context).
#[derive(Clone)]
pub(crate) struct Reaction {
    pub(crate) on_f: Value,
    pub(crate) on_r: Value,
    /// The derived promise, `Value::Undefined` for none, or `Value::Empty` for an `await`
    /// resumption (`on_f` is then the suspended async function's promise).
    pub(crate) result: Value,
    pub(crate) context: Value,
    /// `REACT_*`.
    pub(crate) kind: u8,
    /// A combinator element's index.
    pub(crate) idx: u32,
}

impl Reaction {
    #[inline]
    pub(crate) fn then(on_f: Value, on_r: Value, result: Value, context: Value) -> Reaction {
        Reaction { on_f, on_r, result, context, kind: REACT_THEN, idx: 0 }
    }
}

/// The state of a `Promise.all` / `allSettled` / `any` / `race` call, kept on its result
/// promise: the values (or reasons) list, remainingElementsCount, and the [[AlreadyResolved]]
/// flag of the capability's resolving functions (every settlement of the result goes through
/// it, exactly as calls of those functions would; see [`Interp::combinator_resolve`]).
#[derive(Default)]
pub(crate) struct Combinator {
    pub(crate) values: Vec<Value>,
    pub(crate) remaining: usize,
    pub(crate) already: bool,
}

/// The cell a resolving-function pair shares: the promise and [[AlreadyResolved]].
pub struct ResolverCell {
    pub(crate) promise: Value,
    pub(crate) already: Cell<bool>,
}

/// A promise's internal slots (see the module docs).
pub struct PromiseSlot {
    pub(crate) status: u8,
    /// Listed in `Interp::unhandled_rejections` (cleared when a handler is attached).
    pub(crate) tracked: bool,
    pub(crate) value: Value,
    /// The first pending reaction inline (most promises get at most one), the rest spilled.
    pub(crate) first: Option<Reaction>,
    pub(crate) rest: Vec<Reaction>,
    /// The suspended coroutine of the async function this promise belongs to.
    pub(crate) coro: Option<Box<crate::coroutine::Coroutine>>,
    /// The combinator state when this is a `Promise.all`/`allSettled`/`any` result promise.
    pub(crate) comb: Option<Box<Combinator>>,
}

impl Clone for PromiseSlot {
    /// Cloning copies the observable state only; a coroutine is never duplicated.
    fn clone(&self) -> Self {
        PromiseSlot {
            status: self.status,
            tracked: false,
            value: self.value.clone(),
            first: self.first.clone(),
            rest: self.rest.clone(),
            coro: None,
            comb: None,
        }
    }
}

impl PromiseSlot {
    #[inline]
    pub(crate) fn boxed() -> Box<PromiseSlot> {
        Box::new(PromiseSlot {
            status: PENDING,
            tracked: false,
            value: Value::Undefined,
            first: None,
            rest: Vec::new(),
            coro: None,
            comb: None,
        })
    }

    #[inline]
    pub(crate) fn push_reaction(&mut self, r: Reaction) {
        if self.first.is_none() {
            self.first = Some(r);
        } else {
            if self.rest.capacity() == 0 {
                self.rest.reserve_exact(1);
            }
            self.rest.push(r);
        }
    }

    /// Every object edge the slot holds, for the collector.
    pub(crate) fn object_refs(&self, refs: &mut Vec<Gc>) {
        let mut push = |v: &Value| {
            if let Value::Obj(o) = v {
                refs.push(o.clone());
            }
        };
        push(&self.value);
        for r in self.first.iter().chain(self.rest.iter()) {
            push(&r.on_f);
            push(&r.on_r);
            push(&r.result);
            push(&r.context);
        }
        if let Some(c) = &self.comb {
            for v in &c.values {
                push(v);
            }
        }
    }
}

/// Whether `v` is a promise this engine manages (has [[PromiseState]]).
#[inline]
pub(crate) fn is_promise(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if is_promise_obj(o))
}

#[inline]
pub(crate) fn is_promise_obj(o: &Gc) -> bool {
    matches!(o.borrow().call, Callable::Promise(_))
}

/// `(status, value)` of a promise (a forwarded one reports its target's state).
pub(crate) fn promise_state(v: &Value) -> Option<(u8, Value)> {
    let Value::Obj(o) = v else { return None };
    let b = o.borrow();
    match &b.call {
        Callable::Promise(s) if s.status == FORWARDED => {
            let target = s.value.clone();
            drop(b);
            promise_state(&target)
        }
        Callable::Promise(s) => Some((s.status, s.value.clone())),
        _ => None,
    }
}

/// A realm's promise intrinsics (see [`Interp::promise_intr`]).
pub(crate) struct PromiseIntr {
    global: Gc,
    pub(crate) proto: Gc,
    pub(crate) ctor: Gc,
    /// The original `Promise.prototype.then` and `Promise.resolve`.
    pub(crate) then: Gc,
    pub(crate) resolve: Gc,
    /// Where the pristine checks (`builtins::promise`) found `constructor`, `then` and
    /// `@@species` last time.
    pub(crate) slots: Cell<PristineSlots>,
}

/// Property slots of %Promise.prototype% (`constructor`, `then`) and %Promise% (`@@species`),
/// each trusted only while its object still has the recorded shape: a shape pins which key a
/// slot holds, not its value, so the value is still checked on every use.
#[derive(Clone, Copy, Default)]
pub(crate) struct PristineSlots {
    pub(crate) proto_shape: Option<u32>,
    pub(crate) ctor_slot: u32,
    pub(crate) then_slot: u32,
    pub(crate) ctor_shape: Option<u32>,
    pub(crate) species_slot: u32,
}

#[derive(Default)]
pub(crate) struct PromiseCaches {
    /// The promise intrinsics of the realm last asked for.
    intr: RefCell<Option<Rc<PromiseIntr>>>,
    /// The shared native targets of promise resolving functions (`resolve`, `reject`), and the
    /// own-property map every resolving function starts with (`length` 1, `name` "").
    resolver: RefCell<Option<Props>>,
    /// Set by the microtask drain loop around each job it runs (not by a host's single-job
    /// step), consumed by the job runner.
    pub(crate) drain_job: Cell<bool>,
    /// Set by [`Interp::run_await_job`] when the resumed coroutine is the root of a job the
    /// drain loop is running; consumed by the next `drive_async`.
    job_root: Cell<bool>,
    /// A pending `Call; Await` fusion request: the callee object's address and the call depth
    /// its body runs at (see [`Interp::note_await_call`]); `(0, 0)` when none.
    await_call: Cell<(usize, u32)>,
}

impl Interp {
    /// The current realm's promise intrinsics (`None` while they are being installed).
    #[inline]
    pub(crate) fn promise_intr(&self) -> Option<Rc<PromiseIntr>> {
        if let Some(i) = &*self.lang.promise.intr.borrow() {
            if Gc::ptr_eq(&i.global, &self.global) {
                return Some(i.clone());
            }
        }
        let e = &self.extra_protos;
        let intr = Rc::new(PromiseIntr {
            global: self.global.clone(),
            proto: e.get("Promise")?.clone(),
            ctor: e.get("%PromiseCtor%")?.clone(),
            then: e.get("%Promise.prototype.then%")?.clone(),
            resolve: e.get("%Promise.resolve%")?.clone(),
            slots: Cell::default(),
        });
        *self.lang.promise.intr.borrow_mut() = Some(intr.clone());
        Some(intr)
    }

    /// The current realm's %Promise.prototype%.
    #[inline]
    pub(crate) fn promise_proto(&self) -> Option<Gc> {
        match self.promise_intr() {
            Some(i) => Some(i.proto.clone()),
            None => self.extra_protos.get("Promise").cloned(),
        }
    }

    /// Park the suspended coroutine of the async function whose result promise is `promise`.
    pub(crate) fn park_async_coro(&mut self, promise: &Value, coro: crate::coroutine::Coroutine) {
        Self::park_async_box(promise, Box::new(coro));
    }

    pub(crate) fn park_async_box(promise: &Value, coro: Box<crate::coroutine::Coroutine>) {
        if let Value::Obj(o) = promise {
            if let Callable::Promise(s) = &mut o.borrow_mut().call {
                s.coro = Some(coro);
            }
        }
    }

    /// Release the pin a thread-backed async body's promise holds while the body runs.
    pub(crate) fn unpin_async_thread(&mut self, promise: &Value, coro: &crate::coroutine::Coroutine) {
        if let (crate::coroutine::Coroutine::Thread(_), Value::Obj(o)) = (coro, promise) {
            self.gc_pins.remove(&(Gc::as_ptr(o) as usize));
        }
    }

    pub(crate) fn take_async_coro(promise: &Value) -> Option<Box<crate::coroutine::Coroutine>> {
        match promise {
            Value::Obj(o) => match &mut o.borrow_mut().call {
                Callable::Promise(s) => s.coro.take(),
                _ => None,
            },
            _ => None,
        }
    }

    /// The standard resolving-function pair of `promise` (CreateResolvingFunctions): two
    /// function objects from the cached template (`length` 1, `name` "") sharing one cell.
    pub(crate) fn make_resolver_pair(&mut self, promise: &Value) -> (Value, Value) {
        let cell = Rc::new(ResolverCell {
            promise: promise.clone(),
            already: Cell::new(false),
        });
        self.resolver_pair_for(cell)
    }

    /// The resolving-function objects over an existing cell.
    pub(crate) fn resolver_pair_for(&mut self, cell: Rc<ResolverCell>) -> (Value, Value) {
        if self.lang.promise.resolver.borrow().is_none() {
            let mut props = Props::new();
            props.insert("length", Property::data(Value::Num(1.0), false, false, true));
            props.insert("name", Property::data(Value::str(""), false, false, true));
            *self.lang.promise.resolver.borrow_mut() = Some(props);
        }
        let c = self.lang.promise.resolver.borrow();
        let props = c.as_ref().expect("initialized above");
        let res = Object::new_inline_copy(Some(self.function_proto.clone()), props);
        let rej = Object::new_inline_copy(Some(self.function_proto.clone()), props);
        drop(c);
        res.borrow_mut().call = Callable::Resolver(cell.clone(), true);
        rej.borrow_mut().call = Callable::Resolver(cell, false);
        (Value::Obj(res), Value::Obj(rej))
    }

    /// A call of a resolving function: the first call of either one of a pair settles.
    pub(crate) fn call_promise_resolver(&mut self, cell: &ResolverCell, fulfil: bool, args: &[Value]) {
        if cell.already.replace(true) {
            return;
        }
        let v = args.first().cloned().unwrap_or(Value::Undefined);
        if fulfil {
            self.resolve_promise(&cell.promise, v);
        } else {
            self.reject_promise(&cell.promise, v);
        }
    }

    /// The job for reaction `r` of a promise that settled with `value`.
    #[inline]
    pub(crate) fn reaction_job(r: Reaction, fulfilled: bool, value: Value) -> Job {
        let (handler, kind) = match r.kind {
            REACT_THEN => (if fulfilled { r.on_f } else { r.on_r }, JOB_REACTION),
            k => (Value::Undefined, JOB_COMB_BASE + k),
        };
        Job {
            handler,
            result: r.result,
            value,
            fulfilled,
            kind,
            idx: r.idx,
            context: r.context,
        }
    }

    /// PerformPromiseThen on the native promise `p` with a prepared reaction.
    pub(crate) fn perform_then(&mut self, p: &Gc, reaction: Reaction) {
        let (status, value) = {
            let mut b = p.borrow_mut();
            let Callable::Promise(s) = &mut b.call else { return };
            if s.status == PENDING {
                s.push_reaction(reaction);
                return;
            }
            // Attaching a handler marks the rejection handled (HostPromiseRejectionTracker
            // "handle").
            if s.tracked {
                s.tracked = false;
                drop(b);
                self.unhandled_rejections.remove(&(Gc::as_ptr(p) as usize));
                b = p.borrow_mut();
            }
            let Callable::Promise(s) = &b.call else { return };
            (s.status, s.value.clone())
        };
        let job = Self::reaction_job(reaction, status == FULFILLED, value);
        self.microtasks.push_back(job);
    }

    /// Run a combinator element job (see [`Interp::combinator_settle`]).
    pub(crate) fn run_combinator_job(&mut self, job: Job) {
        let mode = job.kind - JOB_COMB_BASE;
        self.combinator_settle(&job.result, mode, job.fulfilled, job.idx, job.value);
    }

    /// Element `idx` of the combinator (`mode` = `REACT_*`) whose promise is `result` settled:
    /// record it, or settle `result` through the capability cell, as the spec's element / capability
    /// function registered for that side would.
    pub(crate) fn combinator_settle(
        &mut self,
        result: &Value,
        mode: u8,
        fulfilled: bool,
        idx: u32,
        value: Value,
    ) {
        let records = match mode {
            REACT_ALL => fulfilled,
            REACT_ALL_SETTLED => true,
            REACT_ANY => !fulfilled,
            _ => false,
        };
        if !records {
            self.combinator_resolve(result, fulfilled, value);
            return;
        }
        let value = if mode == REACT_ALL_SETTLED {
            let o = self.new_object();
            crate::value::set_data(
                &o,
                "status",
                Value::str(if fulfilled { "fulfilled" } else { "rejected" }),
            );
            crate::value::set_data(&o, if fulfilled { "value" } else { "reason" }, value);
            Value::Obj(o)
        } else {
            value
        };
        self.combinator_record(result, mode == REACT_ANY, Some((idx, value)));
    }

    /// Store element `idx` (when given) and count one element done; the last one settles the
    /// result: fulfilled with the values array, or (`any`) rejected with an AggregateError.
    pub(crate) fn combinator_record(
        &mut self,
        result: &Value,
        any: bool,
        element: Option<(u32, Value)>,
    ) {
        let Value::Obj(o) = result else { return };
        let done = {
            let mut b = o.borrow_mut();
            let Callable::Promise(s) = &mut b.call else { return };
            let Some(c) = s.comb.as_mut() else { return };
            if let Some((idx, value)) = element {
                if let Some(slot) = c.values.get_mut(idx as usize) {
                    *slot = value;
                }
            }
            c.remaining -= 1;
            if c.remaining == 0 {
                Some(std::mem::take(&mut c.values))
            } else {
                None
            }
        };
        let Some(values) = done else { return };
        let arr = self.make_array(values);
        if any {
            let e = match crate::builtins::make_aggregate_error_value(self, arr) {
                Ok(e) | Err(e) => e,
            };
            self.combinator_resolve(result, false, e);
        } else {
            self.combinator_resolve(result, true, arr);
        }
    }

    /// Call the combinator capability's resolve (`fulfil`) or reject function with `v`: only
    /// the first call of either has an effect.
    pub(crate) fn combinator_resolve(&mut self, result: &Value, fulfil: bool, v: Value) {
        let Value::Obj(o) = result else { return };
        {
            let mut b = o.borrow_mut();
            let Callable::Promise(s) = &mut b.call else { return };
            let Some(c) = s.comb.as_mut() else { return };
            if c.already {
                return;
            }
            c.already = true;
            c.values = Vec::new();
        }
        if fulfil {
            self.resolve_promise(result, v);
        } else {
            self.reject_promise(result, v);
        }
    }

    /// PromiseResolveThenableJob. When `then` is the intrinsic `Promise.prototype.then` on a
    /// native promise whose species lookup is silent, the call would only register the fresh
    /// resolving functions as reactions: register a pass-through reaction settling `promise`
    /// instead (no function objects, no derived promise). Otherwise call `then` for real.
    pub(crate) fn run_thenable_job(&mut self, job: Job) {
        let (then, thenable, promise) = (job.handler, job.value, job.result);
        let outer = std::mem::replace(&mut self.async_context, job.context);
        if let Value::Obj(t) = &thenable {
            if self.is_intrinsic_then(&then) && crate::builtins::promise_then_is_silent(self, &thenable) {
                let reaction = Reaction {
                    on_f: Value::Undefined,
                    on_r: Value::Undefined,
                    result: promise,
                    context: self.async_context.clone(),
                    kind: REACT_THEN,
                    idx: 0,
                };
                self.perform_then(t, reaction);
                self.async_context = outer;
                return;
            }
        }
        let (res, rej) = self.make_resolver_pair(&promise);
        if let Err(e) = self.call(then, thenable, &[res, rej.clone()]) {
            let e = crate::interpreter::abrupt_value(e);
            let _ = self.call(rej, Value::Undefined, &[e]);
        }
        self.async_context = outer;
    }

    /// Whether `f` is this realm's original `Promise.prototype.then`.
    pub(crate) fn is_intrinsic_then(&self, f: &Value) -> bool {
        let Value::Obj(f) = f else { return false };
        matches!(self.promise_intr(), Some(i) if Gc::ptr_eq(&i.then, f))
    }

    /// `await v` inside the async function whose result promise is `async_promise`: subscribe
    /// the coroutine to `v` (see the module docs). `Err` is an abrupt completion of the
    /// PromiseResolve step (a poisoned `constructor` getter), thrown at the `await` itself.
    pub(crate) fn await_subscribe(
        &mut self,
        awaited: Value,
        async_promise: &Value,
    ) -> Result<(), Value> {
        if !matches!(awaited, Value::Obj(_)) {
            // PromiseResolve of a non-object is a fresh fulfilled promise; PerformPromiseThen on
            // it queues the reaction at once. Queue it directly.
            self.microtasks.push_back(Job {
                handler: async_promise.clone(),
                result: Value::Empty,
                value: awaited,
                fulfilled: true,
                kind: JOB_REACTION,
                idx: 0,
                context: self.async_context.clone(),
            });
            return Ok(());
        }
        let px = self.promise_resolve_checked(awaited)?;
        self.promise_then_into(&px, async_promise.clone(), async_promise.clone(), Value::Empty);
        Ok(())
    }

    /// Whether PromiseResolve(%Promise%, `p`) on the native promise `p` returns `p` itself
    /// without running user code: no own `constructor`, and it inherits a data property from
    /// the realm's %Promise.prototype% that still holds %Promise% (the spec's SameValue check).
    pub(crate) fn promise_ctor_get_is_silent(&self, p: &Gc) -> bool {
        let Some(intr) = self.promise_intr() else {
            return false;
        };
        {
            let b = p.borrow();
            if !matches!(&b.proto, Some(x) if Gc::ptr_eq(x, &intr.proto))
                || b.props.contains("constructor")
            {
                return false;
            }
        }
        self.proto_ctor_is_intrinsic(&intr)
    }

    /// %Promise.prototype%.constructor is a data property holding %Promise%.
    fn proto_ctor_is_intrinsic(&self, intr: &PromiseIntr) -> bool {
        matches!(intr.proto.borrow().props.get("constructor"),
            Some(c) if !c.accessor() && matches!(c.value(), Value::Obj(f) if Gc::ptr_eq(&f, &intr.ctor)))
    }

    /// The VM is about to execute a call whose result feeds straight into `Op::Await`. If the
    /// callee turns out to be a compiled async function reached directly (same object, one call
    /// level down - never a nested call made by some other callee), it may hand back a
    /// primitive result instead of a promise (see `call_compiled_async`).
    #[inline]
    pub(crate) fn note_await_call(&self, callee: &Value) {
        if let Value::Obj(o) = callee {
            self.lang
                .promise
                .await_call
                .set((Gc::as_ptr(o) as usize, self.depth + 1));
        }
    }

    /// Withdraw an unconsumed fusion request after the call.
    #[inline]
    pub(crate) fn clear_await_call(&self) {
        self.lang.promise.await_call.set((0, 0));
    }

    /// Consume the fusion request if it targets this very call of `fn_obj`.
    pub(crate) fn take_await_call(&self, fn_obj: &Gc) -> bool {
        let (ptr, depth) = self.lang.promise.await_call.get();
        if ptr == 0 {
            return false;
        }
        self.lang.promise.await_call.set((0, 0));
        ptr == Gc::as_ptr(fn_obj) as usize && depth == self.depth
    }

    /// Whether `Get(p, "constructor")` on a fresh promise of this realm (no own properties)
    /// cannot run user code.
    pub(crate) fn fresh_promise_ctor_get_is_silent(&self) -> bool {
        matches!(self.promise_intr(), Some(intr) if self.proto_ctor_is_intrinsic(&intr))
    }

    /// Take the "resumed as the root of a drained job" mark (see [`Interp::await_direct`]).
    #[inline]
    pub(crate) fn take_await_job_root(&self) -> bool {
        self.lang.promise.job_root.replace(false)
    }

    /// `await v` continuing without a suspension, when that is unobservable: the coroutine is
    /// the root of a job the drain loop is running (no caller frame below it), the microtask
    /// queue is empty (the resumption job would be the very next one popped, and nothing runs
    /// between two jobs of one drain), and `v` settles without user code - a non-object, or a
    /// settled native promise whose `constructor` read is silent. Returns the resume signal,
    /// or `None` to take the ordinary suspend-and-subscribe path.
    pub(crate) fn await_direct(&mut self, awaited: &Value) -> Option<crate::coroutine::Resume> {
        if !self.microtasks.is_empty() || self.terminating {
            return None;
        }
        // The skipped job would have been a safe point (see `run_await_job`).
        if self.gc_check_amortized().is_err() {
            return None;
        }
        match awaited {
            Value::Obj(o) => {
                let (status, value, tracked) = match &o.borrow().call {
                    Callable::Promise(s) if s.status == FULFILLED || s.status == REJECTED => {
                        (s.status, s.value.clone(), s.tracked)
                    }
                    _ => return None,
                };
                if !self.promise_ctor_get_is_silent(o) {
                    return None;
                }
                if status == 1 {
                    Some(crate::coroutine::Resume::Next(value))
                } else {
                    // Subscribing would have marked the rejection handled.
                    if tracked {
                        if let Callable::Promise(s) = &mut o.borrow_mut().call {
                            s.tracked = false;
                        }
                        self.unhandled_rejections.remove(&(Gc::as_ptr(o) as usize));
                    }
                    Some(crate::coroutine::Resume::Throw(value))
                }
            }
            v => Some(crate::coroutine::Resume::Next(v.clone())),
        }
    }

    /// Run an await-resumption job (`job.result` is [`Value::Empty`]): re-drive the async
    /// function whose promise is `job.handler` with the awaited settlement. `from_drain`: the
    /// drain loop runs this job (see [`Interp::await_direct`]).
    pub(crate) fn run_await_job(&mut self, job: Job, from_drain: bool) {
        // The resumption stands in for a call of the reaction function: keep its safe point.
        if self.gc_check_amortized().is_err() {
            return;
        }
        let outer = std::mem::replace(&mut self.async_context, job.context);
        let signal = if job.fulfilled {
            crate::coroutine::Resume::Next(job.value)
        } else {
            crate::coroutine::Resume::Throw(job.value)
        };
        self.lang.promise.job_root.set(from_drain);
        self.drive_async(job.handler, signal);
        self.lang.promise.job_root.set(false);
        self.async_context = outer;
    }
}
