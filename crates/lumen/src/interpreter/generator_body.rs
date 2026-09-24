//! Generator calls (sync and async) whose body compiled to bytecode: the generator object is
//! driven by a [`VmCoro`](crate::bytecode::VmCoro) instead of an OS-thread coroutine.
use super::{Abrupt, Env, Interp};
use crate::ast::Function;
use crate::value::{Gc, Value};
use std::rc::Rc;

impl Interp {
    /// A call of the (possibly async) generator function `fn_obj` on the bytecode tier: tier it up like any
    /// call, then — when its body compiled — bind `this`, run the prologue (parameters,
    /// declarations) to its initial suspension and return the generator object. `None`: the
    /// body stays on the tree-walker (`run_generator`).
    pub(super) fn call_compiled_generator(
        &mut self,
        func: &Rc<Function>,
        closure: &Env,
        this: &Value,
        args: &[Value],
        fn_obj: &Gc,
    ) -> Option<Result<Value, Abrupt>> {
        if matches!(self.tier, crate::bytecode::Tier::Interp) || closure.borrow().under_with
        {
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
        // A generator call is an ordinary [[Call]]: new.target is undefined in its body (the
        // compiler refuses bodies that could observe it), `this` binds per strictness.
        let this_val = self.bind_compiled_this(func, &chunk, this.clone(), false);
        let saved_field_init = std::mem::replace(&mut self.in_field_init_code, false);
        let saved_agb = std::mem::replace(&mut self.in_async_gen_body, false);
        let coro = crate::bytecode::VmCoro::new_generator(
            self,
            chunk,
            closure.clone(),
            this_val,
            args,
            func.is_strict,
        );
        self.in_field_init_code = saved_field_init;
        self.in_async_gen_body = saved_agb;
        let mut coro = match coro {
            Ok(c) => crate::coroutine::Coroutine::Vm(c),
            Err(e) => return Some(Err(e)),
        };
        // Every resume (from `next()`) runs outside this call: give it the function's frame.
        coro.set_frame(self.resume_frame_for(func, false));
        // The generator object's [[Prototype]] comes from the function's own `.prototype`.
        let gen_proto = fn_obj.borrow().props.get("prototype").map(|p| p.value());
        let obj = self.make_generator(func.is_async, gen_proto);
        if let Value::Obj(o) = &obj {
            self.gc_pin(o);
            self.generators.insert(Gc::as_ptr(o) as usize, coro);
            if func.is_async {
                self.async_gens.insert(Gc::as_ptr(o) as usize);
            }
        }
        Some(Ok(obj))
    }
}
