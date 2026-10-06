//! `AbortSignal` state and the abort algorithms.
use super::bindings::{AbortSignal, DomException, Event, EventTarget};
use super::target::Listener;
use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Weak;

/// An abort algorithm registered on a signal.
pub(crate) enum AbortStep {
    /// Remove the listener registered with this `signal` option.
    RemoveListener {
        target: Weak<TargetData>,
        receiver: WeakValue,
        removed: Rc<Cell<bool>>,
    },
    /// Abort a signal created by `cloneTransferableSignal` on a later turn.
    Native(Rc<dyn Fn(&mut Ctx, &Value)>),
    /// A native algorithm its owner keeps alive; dropped with the owner.
    Owned(Weak<dyn Fn(&mut Ctx, &Value)>),
}

/// The native state of one `AbortSignal`.
pub struct SignalState {
    pub(crate) aborted: Cell<bool>,
    pub(crate) reason: RefCell<Value>,
    pub(crate) steps: RefCell<Vec<AbortStep>>,
    /// `AbortSignal.any`: the source signals of a dependent signal.
    pub(crate) sources: RefCell<Vec<Value>>,
    /// The dependent signals of a source signal.
    pub(crate) dependants: RefCell<Vec<Value>>,
    pub(crate) composite: Cell<bool>,
    /// Created by `AbortSignal.timeout`: rooted while it has a strong `abort` listener.
    pub(crate) timeout: Cell<bool>,
}

impl SignalState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            aborted: Cell::new(false),
            reason: RefCell::new(Value::Undefined),
            steps: RefCell::new(Vec::new()),
            sources: RefCell::new(Vec::new()),
            dependants: RefCell::new(Vec::new()),
            composite: Cell::new(false),
            timeout: Cell::new(false),
        })
    }
}

/// Timeout signals kept alive by their `abort` listeners (Node's `gcPersistentSignals`).
#[derive(Default)]
struct PersistentSignals(Vec<Value>);

pub(crate) fn signal_state(ctx: &mut Ctx, value: &Value) -> Option<Rc<SignalState>> {
    ctx.with_instance::<AbortSignal, _>(value, |signal| signal.state.clone())
        .ok()
}

/// A new, not yet aborted `AbortSignal`.
pub fn new_signal(ctx: &mut Ctx) -> OpResult<Value> {
    let signal = ctx.new_instance(AbortSignal {
        base: EventTarget::from_data(TargetData::new(None)),
        state: SignalState::new(),
    });
    ctx.set_native_identity_owner::<AbortSignal>(&signal)?;
    Ok(signal)
}

/// `new DOMException(message, name)`.
pub fn dom_exception(ctx: &mut Ctx, message: &str, name: &str) -> Value {
    let exception = DomException::with_name(message, name);
    ctx.new_instance(exception)
}

pub(crate) fn default_reason(ctx: &mut Ctx) -> Value {
    dom_exception(ctx, "This operation was aborted", "AbortError")
}

pub(crate) fn add_listener_removal(
    ctx: &mut Ctx,
    signal: &Rc<SignalState>,
    data: &Rc<TargetData>,
    receiver: &Value,
    listener: &Listener,
) {
    let Some(receiver) = ctx.weak_value(receiver) else {
        return;
    };
    signal.steps.borrow_mut().push(AbortStep::RemoveListener {
        target: Rc::downgrade(data),
        receiver,
        removed: listener.removed.clone(),
    });
}

/// Register a native abort algorithm on `signal`.
pub(crate) fn add_native_step(signal: &SignalState, step: Rc<dyn Fn(&mut Ctx, &Value)>) {
    signal.steps.borrow_mut().push(AbortStep::Native(step));
}

/// Register `step` on `signal` without keeping it alive: it runs only while its owner holds the
/// `Rc`. Steps whose owner is gone are dropped from the signal on every registration, so a
/// long-lived signal does not accumulate the algorithms of finished operations.
pub fn add_owned_step(signal: &SignalState, step: &Rc<dyn Fn(&mut Ctx, &Value)>) {
    let mut steps = signal.steps.borrow_mut();
    steps.retain(|known| !matches!(known, AbortStep::Owned(weak) if weak.strong_count() == 0));
    steps.push(AbortStep::Owned(Rc::downgrade(step)));
}

/// Make `follower` abort when `source` does, with the same reason (the "follow" algorithm of the
/// DOM standard). The returned step is the registration: the caller keeps it for as long as
/// `follower` should follow. `None` when `source` had already aborted (and so `follower` now has).
pub fn follow_signal(
    ctx: &mut Ctx,
    source: &Value,
    follower: &Value,
) -> OpResult<Option<Rc<dyn Fn(&mut Ctx, &Value)>>> {
    let Some(state) = signal_state(ctx, source) else {
        return Err(invalid_arg_type(ctx, "signal", "an instance of AbortSignal", source));
    };
    if state.aborted.get() {
        let reason = state.reason.borrow().clone();
        abort_signal(ctx, follower, reason)?;
        return Ok(None);
    }
    let target = ctx.weak_value(follower).expect("signals are objects");
    let step: Rc<dyn Fn(&mut Ctx, &Value)> = Rc::new(move |ctx: &mut Ctx, reason: &Value| {
        if let Some(follower) = target.upgrade() {
            let _ = abort_signal(ctx, &follower, reason.clone());
        }
    });
    add_owned_step(&state, &step);
    Ok(Some(step))
}

/// Signal abort: set the reason, run the abort algorithms, fire a trusted `abort` event, then
/// abort the dependent signals.
pub fn abort_signal(ctx: &mut Ctx, signal: &Value, reason: Value) -> OpResult<()> {
    let Some(state) = signal_state(ctx, signal) else {
        return Err(invalid_this("AbortSignal"));
    };
    if state.aborted.get() {
        return Ok(());
    }
    state.aborted.set(true);
    *state.reason.borrow_mut() = reason.clone();
    unpersist(ctx, signal);
    let steps = std::mem::take(&mut *state.steps.borrow_mut());
    for step in steps {
        match step {
            AbortStep::RemoveListener {
                target,
                receiver,
                removed,
            } => {
                if let (Some(target), Some(receiver)) = (target.upgrade(), receiver.upgrade()) {
                    EventTarget::remove_flagged(ctx, &receiver, &target, &removed)?;
                }
            }
            AbortStep::Native(step) => step(ctx, &reason),
            AbortStep::Owned(step) => {
                if let Some(step) = step.upgrade() {
                    step(ctx, &reason);
                }
            }
        }
    }
    let event = ctx.new_instance(Event::trusted("abort"));
    EventTarget::dispatch_trusted(ctx, signal, &event)?;
    let dependants = state.dependants.borrow().clone();
    for dependant in dependants {
        abort_signal(ctx, &dependant, reason.clone())?;
    }
    Ok(())
}

/// A timeout signal gains a strong `abort` listener: keep it alive until it aborts.
pub(crate) fn listener_added(ctx: &mut Ctx, receiver: &Value, kind: &str, weak: bool) {
    if kind != "abort" || weak {
        return;
    }
    let Some(state) = signal_state(ctx, receiver) else {
        return;
    };
    if !state.timeout.get() || state.aborted.get() {
        return;
    }
    if !ctx.op_state().has::<PersistentSignals>() {
        ctx.op_state().put(PersistentSignals::default());
    }
    let signals = ctx.op_state().get_mut::<PersistentSignals>().unwrap();
    if !signals.0.iter().any(|signal| same(signal, receiver)) {
        signals.0.push(receiver.clone());
    }
}

/// A timeout signal lost its last `abort` listener: let it collect.
pub(crate) fn listener_removed(ctx: &mut Ctx, receiver: &Value, kind: &str, size: usize) {
    if kind == "abort" && size == 0 {
        unpersist(ctx, receiver);
    }
}

fn unpersist(ctx: &mut Ctx, signal: &Value) {
    if let Some(signals) = ctx.op_state().get_mut::<PersistentSignals>() {
        signals.0.retain(|held| !same(held, signal));
    }
}

/// `AbortSignal.timeout(delay)`: a signal aborted with a `TimeoutError` after `delay` ms. The
/// timer holds the signal weakly and does not keep the loop alive.
pub(crate) fn timeout_signal(ctx: &mut Ctx, delay: f64) -> OpResult<Value> {
    let signal = new_signal(ctx)?;
    let state = signal_state(ctx, &signal).expect("new AbortSignal");
    state.timeout.set(true);
    let weak = ctx.weak_value(&signal).expect("signals are objects");
    let fire = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
            if let Some(signal) = weak.upgrade() {
                let reason = dom_exception(
                    ctx,
                    "The operation was aborted due to timeout",
                    "TimeoutError",
                );
                abort_signal(ctx, &signal, reason).map_err(|error| error.to_value(ctx))?;
            }
            Ok(Value::Undefined)
        }),
    );
    let global = ctx.global_object();
    let set_timeout = ctx
        .member_get(&global, "setTimeout")
        .map_err(OpError::thrown)?;
    if !set_timeout.is_callable() {
        return Err(OpError::type_error(
            "AbortSignal.timeout requires setTimeout",
        ));
    }
    let timer = ctx
        .invoke(set_timeout, global, &[fire, Value::Num(delay)])
        .map_err(OpError::thrown)?;
    if matches!(timer, Value::Obj(_)) {
        let unref = ctx.member_get(&timer, "unref").map_err(OpError::thrown)?;
        if unref.is_callable() {
            ctx.invoke(unref, timer, &[]).map_err(OpError::thrown)?;
        }
    }
    Ok(signal)
}

/// `AbortSignal.any(signals)`.
pub(crate) fn any_signal(ctx: &mut Ctx, signals: Value) -> OpResult<Value> {
    if !ctx.is_array_value(&signals).map_err(OpError::thrown)? {
        return Err(invalid_arg_type(ctx, "signals", "an instance of Array", &signals));
    }
    let length = ctx.member_get(&signals, "length").map_err(OpError::thrown)?;
    let length = ctx.coerce_number(&length).map_err(OpError::thrown)? as usize;
    let mut inputs = Vec::with_capacity(length);
    for index in 0..length {
        let signal = ctx
            .member_get(&signals, &index.to_string())
            .map_err(OpError::thrown)?;
        let Some(state) = signal_state(ctx, &signal) else {
            return Err(invalid_arg_type(
                ctx,
                &format!("signals[{index}]"),
                "an instance of AbortSignal",
                &signal,
            ));
        };
        inputs.push((signal, state));
    }
    let result = new_signal(ctx)?;
    let result_state = signal_state(ctx, &result).expect("new AbortSignal");
    result_state.composite.set(true);
    if let Some((_, aborted)) = inputs.iter().find(|(_, state)| state.aborted.get()) {
        let reason = aborted.reason.borrow().clone();
        result_state.aborted.set(true);
        *result_state.reason.borrow_mut() = reason;
        return Ok(result);
    }
    let link = |source: &Value, state: &SignalState| {
        let mut sources = result_state.sources.borrow_mut();
        if sources.iter().any(|known| same(known, source)) {
            return;
        }
        sources.push(source.clone());
        state.dependants.borrow_mut().push(result.clone());
    };
    for (signal, state) in &inputs {
        if !state.composite.get() {
            link(signal, state);
            continue;
        }
        let sources = state.sources.borrow().clone();
        for source in sources {
            if let Some(source_state) = signal_state(ctx, &source) {
                link(&source, &source_state);
            }
        }
    }
    Ok(result)
}

/// The copy of a transferable signal that arrives through a `MessagePort`: aborted already if
/// the original is, otherwise aborted on a later turn after the original.
pub(crate) fn clone_transferable(ctx: &mut Ctx, signal: &Value) -> OpResult<Value> {
    let Some(state) = signal_state(ctx, signal) else {
        return Err(invalid_arg_type(ctx, "signal", "an instance of AbortSignal", signal));
    };
    let clone = new_signal(ctx)?;
    let key = ctx.symbol_for(TRANSFERABLE_SIGNAL);
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    ctx.member_set(&descriptor, "value", Value::Bool(true))
        .map_err(OpError::thrown)?;
    ctx.define_property_value(&clone, key, &descriptor)
        .map_err(OpError::thrown)?;
    if state.aborted.get() {
        let reason = state.reason.borrow().clone();
        abort_signal(ctx, &clone, reason)?;
        return Ok(clone);
    }
    let target = ctx.weak_value(&clone).expect("signals are objects");
    add_native_step(
        &state,
        Rc::new(move |ctx: &mut Ctx, reason: &Value| {
            let Some(clone) = target.upgrade() else {
                return;
            };
            let reason = reason.clone();
            let fire = ctx.new_native_fn(
                "",
                0,
                Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                    abort_signal(ctx, &clone, reason.clone()).map_err(|error| error.to_value(ctx))?;
                    Ok(Value::Undefined)
                }),
            );
            let global = ctx.global_object();
            let schedule = ["setImmediate", "setTimeout"]
                .into_iter()
                .find_map(|name| ctx.member_get(&global, name).ok().filter(Value::is_callable));
            let Some(schedule) = schedule else {
                ctx.queue_microtask(fire);
                return;
            };
            let Ok(timer) = ctx.invoke(schedule, global, &[fire, Value::Num(0.0)]) else {
                return;
            };
            if matches!(timer, Value::Obj(_)) {
                if let Ok(unref) = ctx.member_get(&timer, "unref") {
                    if unref.is_callable() {
                        let _ = ctx.invoke(unref, timer, &[]);
                    }
                }
            }
        }),
    );
    Ok(clone)
}
