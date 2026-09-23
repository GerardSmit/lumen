//! The call-frame stack behind error stacks and legacy `fn.caller`/`fn.arguments` reflection.
use super::Env;
use crate::value::{Gc, Value};
use std::cell::RefCell;
use std::rc::Rc;

/// One entry of the legacy `fn.caller`/`fn.arguments` reflection stack (see `call_user`). The
/// arguments object materializes lazily: a body that never names `arguments` skips building it,
/// and `lazy` keeps what a later reflective read needs to conjure it on demand.
pub struct FnFrame {
    /// `Rc::as_ptr` of the callee. No strong handle is kept: every frame is pushed while its
    /// caller holds the callee alive (the callee `Value` sits on the caller's operand stack or in
    /// the dispatch chain for the whole call — for frames owned by a parked coroutine, the
    /// worker's frozen stack; a torn-down coroutine's worker parks forever rather than unwinding,
    /// which this invariant depends on), so the rare reflective reads reconstruct one via
    /// [`FnFrame::callee`] instead of paying a refcount round-trip on every call.
    pub fn_ptr: usize,
    /// Owning coroutine body (`Interp::cur_coro`; 0 = the main driver): a worker-thread panic
    /// evicts the dead body's frames by this tag (see `ThreadCoro::resume`).
    pub coro: u32,
    pub strict: bool,
    /// The rare per-frame state (a live `arguments` object, or what a reflective `fn.arguments`
    /// read needs to conjure one). Boxed so the common frame stays compact — frames are pushed
    /// and popped on EVERY call, and the pop's copy-out and drop-check of a fat frame was a
    /// measurable slice of the call path.
    pub extra: Option<Box<FrameExtra>>,
}

/// See [`FnFrame::extra`].
pub struct FrameExtra {
    pub args_obj: Value,
    pub lazy: Option<(Rc<crate::ast::Function>, Rc<[Value]>, Env)>,
}

impl Default for FrameExtra {
    fn default() -> FrameExtra {
        FrameExtra {
            args_obj: Value::Null,
            lazy: None,
        }
    }
}

impl FnFrame {
    /// A strong handle to the callee, reconstructed from `fn_ptr` (see its aliveness invariant).
    pub fn callee(&self) -> Gc {
        let p = self.fn_ptr as *const RefCell<crate::value::Object>;
        unsafe {
            Gc::increment_strong_count(p);
            Gc::from_raw(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn fresh_activation_closure_reflects_the_actual_cached_call_target() {
        let source = r#"
            function inspect() { eval(''); return inspect.caller; }
            function factory() {
                return function() {
                    let captured = 1;
                    function inner() { return captured; }
                    if (inner() !== 1) throw 'capture';
                    return inspect();
                };
            }
            function invoke(f) { return f(); }
            const first = factory();
            for (let i = 0; i < 500; i++) {
                if (invoke(first) !== first) throw 'warm caller';
            }
            for (let i = 0; i < 10; i++) {
                const next = factory();
                if (invoke(next) !== next) throw 'fresh caller';
            }
            'passed'
        "#;
        for tier in [Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed"),
                Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
            assert!(engine.interp.fn_frames.is_empty());
        }
    }
}
