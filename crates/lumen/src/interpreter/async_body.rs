//! Async function calls whose body compiled to bytecode: the lean entry (no activation scope,
//! no string-keyed parameter binding) straight into a [`VmCoro`](crate::bytecode::VmCoro), the
//! async counterpart of `call_compiled_generator`.
use super::{Abrupt, Env, Interp};
use crate::ast::Function;
use crate::value::{Gc, Value};
use std::rc::Rc;

impl Interp {
    /// A call of the async (non-generator) function `func` on the bytecode tier: tier it up like
    /// any call, then — when its body compiled — bind `this`, create the result promise and run
    /// the body to its first suspension. `None`: the body stays on the tree-walker path
    /// (`run_async`, which may still pick the VM with a tree-walker activation).
    pub(super) fn call_compiled_async(
        &mut self,
        func: &Rc<Function>,
        closure: &Env,
        this: &Value,
        args: &[Value],
        fn_obj: &Gc,
    ) -> Option<Result<Value, Abrupt>> {
        if matches!(self.tier, crate::bytecode::Tier::Interp) || closure.borrow().under_with {
            return None;
        }
        if func.code.get().is_none() {
            let n = func.calls.get().saturating_add(1);
            func.calls.set(n);
            if n > self.tier_threshold || func.scan_flags() & crate::ast::SCAN_HAS_LOOP != 0 {
                let _ = func.code.set(crate::bytecode::compile(func));
            }
        }
        let chunk = match func.code.get() {
            Some(Some(c)) => c.clone(),
            _ => return None,
        };
        // An async function call is an ordinary [[Call]] (async functions are not
        // constructors); parameters seed straight into slots and their defaults run in the
        // body's prologue, so a throw there rejects the promise like any body throw.
        let this_val = self.bind_compiled_this(func, &chunk, this.clone(), false);
        let saved_strict = std::mem::replace(&mut self.strict, func.is_strict);
        let saved_field_init = self.in_field_init_code;
        let saved_agb = self.in_async_gen_body;
        if !func.is_arrow {
            self.in_field_init_code = false;
        }
        self.in_async_gen_body = false;
        // Consume a `Call; Await` fusion request aimed at exactly this call (see
        // `note_await_call`).
        let fused = self.take_await_call(fn_obj);
        let mut coro = crate::coroutine::Coroutine::Vm(crate::bytecode::VmCoro::new(
            self,
            chunk,
            closure.clone(),
            this_val,
            args,
        ));
        // The first step runs before the promise exists: nothing in the body can reach its
        // own result promise, so creating it afterwards is unobservable.
        let suspend = coro.resume(self, crate::coroutine::Resume::Next(Value::Undefined));
        self.strict = saved_strict;
        self.in_field_init_code = saved_field_init;
        self.in_async_gen_body = saved_agb;
        use crate::coroutine::Suspend;
        // `await f()` of a body that returned a primitive without suspending: the promise would
        // be fulfilled at once and only ever awaited, and awaiting it (a silent `constructor`
        // read, then one reaction job) is exactly awaiting the value itself. Skip the promise.
        if fused {
            if let Suspend::Done(v) = &suspend {
                if !matches!(v, Value::Obj(_)) && self.fresh_promise_ctor_get_is_silent() {
                    return Some(Ok(v.clone()));
                }
            }
        }
        let promise = self.new_promise();
        match suspend {
            Suspend::Await(awaited) => {
                // Its later resumes run outside this call: give them the function's frame.
                coro.set_frame(self.resume_frame_for(func, false));
                // The suspended body lives in its promise's slot.
                self.park_async_coro(&promise, coro);
                if let Err(e) = self.await_subscribe(awaited, &promise) {
                    self.drive_async(promise.clone(), crate::coroutine::Resume::Throw(e));
                }
            }
            Suspend::Yield(v) | Suspend::Done(v) => self.resolve_promise(&promise, v),
            Suspend::Throw(e) => self.reject_promise(&promise, e),
        }
        Some(Ok(promise))
    }
}
