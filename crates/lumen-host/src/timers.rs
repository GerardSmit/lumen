//! Scheduling a native callback through the realm's own `setTimeout`, so native web classes
//! (XMLHttpRequest timeouts, EventSource reconnection) share the realm's timer queue, clamping
//! and event-loop liveness instead of keeping a second clock.

use lumen::embed::{Ctx, OpError, OpResult, Value};
use std::rc::Rc;

/// An armed timer: the handle `setTimeout` returned (a `Timeout` object or a number).
#[derive(Clone)]
pub struct Timer(Value);

impl Timer {
    /// Stop the timer. Harmless when it already fired.
    pub fn clear(&self, ctx: &mut Ctx) {
        let global = ctx.global_object();
        let Ok(clear) = ctx.member_get(&global, "clearTimeout") else {
            return;
        };
        if clear.is_callable() {
            let _ = ctx.invoke(clear, global, std::slice::from_ref(&self.0));
        }
    }

    /// Let the event loop exit while this timer is pending (Node's `timeout.unref()`); a no-op
    /// where the handle has no `unref`.
    pub fn unref(&self, ctx: &mut Ctx) {
        if !matches!(self.0, Value::Obj(_)) {
            return;
        }
        if let Ok(unref) = ctx.member_get(&self.0, "unref") {
            if unref.is_callable() {
                let _ = ctx.invoke(unref, self.0.clone(), &[]);
            }
        }
    }
}

/// Run `callback` once after `delay_ms` milliseconds. The closure holds whatever it captures; a
/// callback that must not keep an object alive captures a weak reference.
pub fn set_timeout(
    ctx: &mut Ctx,
    delay_ms: f64,
    callback: impl Fn(&mut Ctx) + 'static,
) -> OpResult<Timer> {
    let function = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
            callback(ctx);
            Ok(Value::Undefined)
        }),
    );
    let global = ctx.global_object();
    let schedule = ctx.member_get(&global, "setTimeout").map_err(OpError::thrown)?;
    if !schedule.is_callable() {
        return Err(OpError::type_error("setTimeout is unavailable"));
    }
    let handle = ctx
        .invoke(schedule, global, &[function, Value::Num(delay_ms)])
        .map_err(OpError::thrown)?;
    Ok(Timer(handle))
}
