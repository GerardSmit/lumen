//! `vm` `timeout`: run a function under a deadline. A watchdog thread raises the realm's
//! script-timeout flag when the deadline passes; every safe point (call, loop turn) then throws
//! until the run has unwound here, where the throw becomes Node's
//! `ERR_SCRIPT_EXECUTION_TIMEOUT`. Deadlines nest: an inner run that unwinds because an
//! *enclosing* deadline fired leaves the flag raised so the enclosing run unwinds too. A run with
//! `breakOnSigint` is stopped the same way by SIGINT and ends in `ERR_SCRIPT_EXECUTION_INTERRUPTED`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_common::limits::Deadline;
use lumen_host::{ops, Ctx, OpDecl, Value};

use crate::signals::SigintBreak;

pub const VM_OPS: &[OpDecl] = ops![
    "runWithTimeout" (3) => op_run_with_timeout,
];

/// What stopped a bounded run: the deadline, or SIGINT.
#[derive(Default)]
struct Fired {
    timeout: Arc<AtomicBool>,
    sigint: Arc<AtomicBool>,
}

impl Fired {
    fn any(&self) -> bool {
        self.timeout.load(Ordering::SeqCst) || self.sigint.load(Ordering::SeqCst)
    }
}

thread_local! {
    /// The runs active on this realm's thread, outermost first.
    static ACTIVE: RefCell<Vec<Arc<Fired>>> = const { RefCell::new(Vec::new()) };
}

/// `(timeoutMs, fn, breakOnSigint)` — call `fn()`; if the deadline passes first (or SIGINT
/// arrives, with `breakOnSigint`), throw the matching error.
fn op_run_with_timeout(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let ms = args.first().and_then(Value::as_num_opt).unwrap_or(0.0);
    let callee = args.get(1).cloned().unwrap_or(Value::Undefined);
    let break_on_sigint = matches!(args.get(2), Some(Value::Bool(true)));
    let timed = ms.is_finite() && ms > 0.0;
    if !timed && !break_on_sigint {
        return ctx.invoke(callee, Value::Undefined, &[]);
    }
    let raised = ctx.script_timeout_flag();
    let sigint = break_on_sigint.then(|| SigintBreak::new(Arc::clone(&raised)));
    let fired = Arc::new(Fired {
        timeout: Arc::new(AtomicBool::new(false)),
        sigint: sigint.as_ref().map_or_else(|| Arc::new(AtomicBool::new(false)), SigintBreak::fired_flag),
    });
    let watchdog = timed.then(|| {
        let (raised, fired) = (Arc::clone(&raised), Arc::clone(&fired.timeout));
        Deadline::start("lumen-vm-timeout", Duration::from_secs_f64(ms / 1000.0), move || {
            // `fired` first: the unwinding side reads it after seeing the flag.
            fired.store(true, Ordering::SeqCst);
            raised.store(true, Ordering::SeqCst);
        })
    });
    ACTIVE.with(|a| a.borrow_mut().push(Arc::clone(&fired)));
    let result = ctx.invoke(callee, Value::Undefined, &[]);
    drop(watchdog);
    drop(sigint);
    ACTIVE.with(|a| a.borrow_mut().pop());
    // Lower the flag unless an enclosing run has been stopped as well (re-checked after lowering,
    // so a stop that lands in between is not lost).
    let outer_fired = || ACTIVE.with(|a| a.borrow().iter().any(|f| f.any()));
    if !outer_fired() {
        raised.store(false, Ordering::SeqCst);
        if outer_fired() {
            raised.store(true, Ordering::SeqCst);
        }
    }
    match result {
        Err(_) if fired.timeout.load(Ordering::SeqCst) => {
            let err = ctx.make_error("Error", format!("Script execution timed out after {ms}ms"));
            let _ = ctx.set_member(&err, "code", Value::str("ERR_SCRIPT_EXECUTION_TIMEOUT"));
            Err(err)
        }
        Err(_) if fired.sigint.load(Ordering::SeqCst) => {
            let err = ctx.make_error("Error", "Script execution was interrupted by `SIGINT`");
            let _ = ctx.set_member(&err, "code", Value::str("ERR_SCRIPT_EXECUTION_INTERRUPTED"));
            Err(err)
        }
        other => other,
    }
}
