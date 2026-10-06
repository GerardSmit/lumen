//! `vm` `timeout`: run a function under a deadline. A scheduler timer raises the realm's
//! script-timeout flag when the deadline passes; every safe point (call, loop turn) then throws
//! until the run has unwound here, where the throw becomes Node's
//! `ERR_SCRIPT_EXECUTION_TIMEOUT`. Deadlines nest: an inner run that unwinds because an
//! *enclosing* deadline fired leaves the flag raised so the enclosing run unwinds too. A run with
//! `breakOnSigint` is stopped the same way by SIGINT and ends in `ERR_SCRIPT_EXECUTION_INTERRUPTED`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen::embed::OpError;
use lumen_host::{Ctx, Value};
use lumen_os::sched::Deadline;

use crate::signals::SigintBreak;

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__vm")]
pub(crate) mod bindings {
    use super::*;

    /// `(timeoutMs, fn, breakOnSigint)`: call `fn()`; if the deadline passes first (or SIGINT
    /// arrives, with `breakOnSigint`), throw the matching error.
    #[op(coerce, name = "runWithTimeout")]
    pub fn run_with_timeout(ctx: &mut Ctx, ms: f64, callee: Value, break_on_sigint: bool) -> Result<Value, OpError> {
        run_bounded(ctx, ms, callee, break_on_sigint)
    }
}

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

fn run_bounded(ctx: &mut Ctx, ms: f64, callee: Value, break_on_sigint: bool) -> Result<Value, OpError> {
    let timed = ms.is_finite() && ms > 0.0;
    if !timed && !break_on_sigint {
        return ctx.invoke(callee, Value::Undefined, &[]).map_err(OpError::thrown);
    }
    let raised = ctx.script_timeout_flag();
    let sigint = break_on_sigint.then(|| SigintBreak::new(Arc::clone(&raised)));
    let fired = Arc::new(Fired {
        timeout: Arc::new(AtomicBool::new(false)),
        sigint: sigint
            .as_ref()
            .map_or_else(|| Arc::new(AtomicBool::new(false)), SigintBreak::fired_flag),
    });
    let watchdog = timed.then(|| {
        let (raised, fired) = (Arc::clone(&raised), Arc::clone(&fired.timeout));
        Deadline::start(
            "lumen-vm-timeout",
            Duration::from_secs_f64(ms / 1000.0),
            move || {
                // `fired` first: the unwinding side reads it after seeing the flag.
                fired.store(true, Ordering::SeqCst);
                raised.store(true, Ordering::SeqCst);
            },
        )
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
            Err(OpError::error(format!("Script execution timed out after {ms}ms")).with_code("ERR_SCRIPT_EXECUTION_TIMEOUT"))
        }
        Err(_) if fired.sigint.load(Ordering::SeqCst) => {
            Err(OpError::error("Script execution was interrupted by `SIGINT`").with_code("ERR_SCRIPT_EXECUTION_INTERRUPTED"))
        }
        Err(thrown) => Err(OpError::thrown(thrown)),
        Ok(v) => Ok(v),
    }
}
