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
    /// `lazy` was stashed by a compiled frame (`bytecode::reflect`): its parameters live in
    /// slots, so the conjured object is an unmapped snapshot.
    pub unmapped: bool,
}

impl Default for FrameExtra {
    fn default() -> FrameExtra {
        FrameExtra {
            args_obj: Value::Null,
            lazy: None,
            unmapped: false,
        }
    }
}

impl FnFrame {
    /// Snapshot only callee identity; the hidden local owns the function throughout
    /// the inline. Weak snapshots neither prolong reachability nor change frame ABI.
    pub(crate) fn record_inline(
        &mut self,
        state: *const InlineFrame,
        mut local: impl FnMut(u16) -> Value,
    ) {
        self.inline = state;
        // The executing activation owns the immutable metadata chain.
        let mut current = unsafe { state.as_ref() };
        if !current.is_some_and(|inline| inline.dynamic) {
            return;
        }
        while let Some(inline) = current {
            if let Some(slot) = inline.callee_slot {
                let Value::Obj(callee) = local(slot) else {
                    panic!("active inline callee has no owning local");
                };
                let extra = self.extra.get_or_insert_with(Default::default);
                match extra
                    .inline_callees
                    .iter_mut()
                    .find(|(key, _)| *key == slot)
                {
                    Some((_, pin)) if pin.as_ptr() == Rc::as_ptr(&callee) => {}
                    Some((_, pin)) => *pin = Rc::downgrade(&callee),
                    None => extra.inline_callees.push((slot, Rc::downgrade(&callee))),
                }
            }
            current = inline.parent.as_deref();
        }
    }

    fn inline_callee<'a>(
        &'a self,
        inline: &'a InlineFrame,
    ) -> Option<&'a Weak<RefCell<crate::value::Object>>> {
        match inline.callee_slot {
            None => Some(&inline.callee),
            Some(slot) => self
                .extra
                .as_ref()?
                .inline_callees
                .iter()
                .find(|(key, _)| *key == slot)
                .map(|(_, callee)| callee),
        }
    }
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
