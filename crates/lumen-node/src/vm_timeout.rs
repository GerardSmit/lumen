//! `vm` `timeout`: run a function under a deadline. A watchdog thread raises the realm's
//! script-timeout flag when the deadline passes; every safe point (call, loop turn) then throws
//! until the run has unwound here, where the throw becomes Node's
//! `ERR_SCRIPT_EXECUTION_TIMEOUT`. Deadlines nest: an inner run that unwinds because an
//! *enclosing* deadline fired leaves the flag raised so the enclosing run unwinds too.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_common::limits::Deadline;
use lumen_host::{ops, Ctx, OpDecl, Value};

pub const VM_OPS: &[OpDecl] = ops![
    "runWithTimeout" (2) => op_run_with_timeout,
];

thread_local! {
    /// The `fired` flags of the deadlines active on this realm's thread, outermost first.
    static ACTIVE: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
}

/// `(timeoutMs, fn)` — call `fn()`; if the deadline passes first, throw the timeout error.
fn op_run_with_timeout(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let ms = args.first().and_then(Value::as_num_opt).unwrap_or(0.0);
    let callee = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !(ms.is_finite() && ms > 0.0) {
        return ctx.invoke(callee, Value::Undefined, &[]);
    }
    let raised = ctx.script_timeout_flag();
    let fired = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (raised, fired) = (Arc::clone(&raised), Arc::clone(&fired));
        Deadline::start("lumen-vm-timeout", Duration::from_secs_f64(ms / 1000.0), move || {
            // `fired` first: the unwinding side reads it after seeing the flag.
            fired.store(true, Ordering::SeqCst);
            raised.store(true, Ordering::SeqCst);
        })
    };
    ACTIVE.with(|a| a.borrow_mut().push(Arc::clone(&fired)));
    let result = ctx.invoke(callee, Value::Undefined, &[]);
    drop(watchdog);
    ACTIVE.with(|a| a.borrow_mut().pop());
    // Lower the flag unless an enclosing deadline has fired as well (re-checked after lowering,
    // so a deadline that fires in between is not lost).
    let outer_fired = || ACTIVE.with(|a| a.borrow().iter().any(|f| f.load(Ordering::SeqCst)));
    if !outer_fired() {
        raised.store(false, Ordering::SeqCst);
        if outer_fired() {
            raised.store(true, Ordering::SeqCst);
        }
    }
    match result {
        Err(_) if fired.load(Ordering::SeqCst) => {
            let err = ctx.make_error("Error", format!("Script execution timed out after {ms}ms"));
            let _ = ctx.set_member(&err, "code", Value::str("ERR_SCRIPT_EXECUTION_TIMEOUT"));
            Err(err)
        }
        other => other,
    }
}
