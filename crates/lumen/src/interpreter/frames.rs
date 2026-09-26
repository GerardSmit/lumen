//! The call-frame stack behind error stacks and legacy `fn.caller`/`fn.arguments` reflection.
use super::Env;
use crate::value::{Gc, Value};
use std::cell::RefCell;
use std::rc::Rc;

/// One entry of the call stack behind error stacks and the legacy `fn.caller`/`fn.arguments`
/// reflection (see `call_user` and `bytecode::reflect`).
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
    /// The caller's call site when this frame was pushed (`Interp::cur_site` at the time; see
    /// [`NO_SITE`]), put back into `cur_site` when it pops. A stack trace shows it as the
    /// CALLER's position; the innermost frame's own is `cur_site`.
    pub caller_site: u32,
    pub strict: bool,
    /// Entered through [[Construct]]: a stack trace names the frame `new F`.
    pub construct: bool,
    /// The rare per-frame state (what a reflective `fn.arguments` read needs, or a pseudo
    /// frame's script). Boxed so the common frame stays compact — frames are pushed
    /// and popped on EVERY call, and the pop's copy-out and drop-check of a fat frame was a
    /// measurable slice of the call path.
    pub extra: Option<Box<FrameExtra>>,
}

/// No call site: a frame that has not called anything yet (or a caller outside any code).
pub const NO_SITE: u32 = u32::MAX;
/// Tag of a site that is a bytecode pc (the low bits) of the frame's own chunk, set by the VM
/// and native code at their call ops; an untagged site is a source position the tree-walker
/// set (`ast::Expr::Call`'s `pos`). See `Interp::cur_site`.
pub const SITE_PC: u32 = 1 << 31;

/// Tag on `Interp::cur_site` meaning "the next callee's caller is hidden": set while a native
/// function runs (see `Interp::dispatch_native`) and for a proper tail call, whose strict caller
/// has left the stack. The callee records it in its [`FnFrame::caller_site`], and `fn.caller`
/// then reports null (as V8 does for a builtin or strict caller). [`NO_SITE`] already has the
/// bit; [`site_pos`] strips it for position decoding.
pub const SITE_NATIVE: u32 = 1 << 30;

/// A native function that is running, on the Rust stack of its dispatcher
/// (`Interp::dispatch_native`, `call_native_fast`, `bytecode::call_native_prepared`) and linked
/// from `Interp::native_top` while it runs, innermost first. A stack trace prints V8's
/// `at Array.map (<anonymous>)` frame for it between the JS frames around it (see
/// `stack_trace::capture_trace`).
pub struct NativeCtx {
    /// The next outer running native (`Interp::native_top` before this one; 0 = none).
    pub prev: usize,
    /// `Interp::cur_site` when it was called: the JS caller's call site, or (called by another
    /// native) that native's [`SITE_NATIVE`]-tagged one.
    pub site: u32,
    /// Entered through [[Construct]] (`new Promise`).
    pub construct: bool,
    /// A call adaptor V8 does not show (`Function.prototype.call`, see
    /// `bytecode::reflect::native_transparent`).
    pub hidden: std::cell::Cell<bool>,
    /// The `NativeFn` address (0: a data-carrying native closure, which has no frame).
    pub id: usize,
    /// What the receiver is (`RECV_*`): a primitive's kind names the frame (`String.replace`)
    /// with no reference to the value.
    pub recv_kind: u8,
    /// An object receiver, alive for the call (it names the frame: `Array.map`, `A.map`), or
    /// null. The dispatchers hold one only once [`RECORD_RECEIVERS`] is set (a refcount round
    /// trip per native call otherwise); until then the frame is named by the function's home
    /// (`Array` for `Array.prototype.map`).
    pub this: *const Value,
}

/// `NativeCtx::recv_kind`: `undefined`/`null` (V8 prints the bare name: `String`).
pub const RECV_NONE: u8 = 0;
/// `NativeCtx::recv_kind`: an object.
pub const RECV_OBJ: u8 = 1;
/// `NativeCtx::recv_kind` of a primitive: `RECV_PRIM + i` names `PRIM_NAMES[i]`.
pub const RECV_PRIM: u8 = 2;
/// Wrapper names of the primitive receiver kinds (see [`RECV_PRIM`]).
pub const PRIM_NAMES: [&str; 5] = ["String", "Number", "Boolean", "Symbol", "BigInt"];

/// Set by the first stack trace that names a native frame by its receiver: from then on the
/// native dispatchers keep an object receiver alive for the call (see [`NativeCtx::this`]), so
/// a program that never takes a trace through a builtin pays no refcount traffic for it. Only
/// that first trace can name a subclass or borrowed-method receiver by the method's home.
pub static RECORD_RECEIVERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`NativeCtx::recv_kind`] of `v`.
#[inline(always)]
pub fn recv_kind(v: &Value) -> u8 {
    match v {
        Value::Obj(_) => RECV_OBJ,
        Value::Str(_) => RECV_PRIM,
        Value::Num(_) => RECV_PRIM + 1,
        Value::Bool(_) => RECV_PRIM + 2,
        Value::Sym(_) => RECV_PRIM + 3,
        Value::BigInt(_) => RECV_PRIM + 4,
        Value::Undefined | Value::Null | Value::Empty => RECV_NONE,
    }
}

/// What a dispatcher holds of an owned receiver it hands to the native: a clone of an object
/// receiver once [`RECORD_RECEIVERS`] is set, else `undefined` (see [`NativeCtx::this`]).
#[inline(always)]
pub fn hold_receiver(v: &Value) -> Value {
    match v {
        Value::Obj(_) if RECORD_RECEIVERS.load(std::sync::atomic::Ordering::Relaxed) => v.clone(),
        _ => Value::Undefined,
    }
}

/// [`NativeCtx::this`] for what [`hold_receiver`] returned.
#[inline(always)]
pub fn held_ptr(held: &Value) -> *const Value {
    match held {
        Value::Obj(_) => held,
        _ => std::ptr::null(),
    }
}

/// `site` without the [`SITE_NATIVE`] tag (for position decoding).
#[inline]
pub fn site_pos(site: u32) -> u32 {
    if site == NO_SITE {
        site
    } else {
        site & !SITE_NATIVE
    }
}

/// Whether `site` was recorded while a native function was running (see [`SITE_NATIVE`]).
#[inline]
pub fn site_is_native(site: u32) -> bool {
    site != NO_SITE && site & SITE_NATIVE != 0
}

/// See [`FnFrame::extra`].
#[derive(Default)]
pub struct FrameExtra {
    /// What a reflective `fn.arguments` read of this activation needs (see
    /// `bytecode::reflect`), recorded only once some such read has happened.
    pub reflect: Option<ReflectStash>,
    /// Set on a *pseudo* frame (`fn_ptr` 0): top-level script, module or eval code, which has no
    /// function object but is a line of a stack trace (see `stack_trace`). Reflection
    /// (`fn.caller`) skips pseudo frames.
    pub script: Option<Rc<super::stack_trace::ScriptFrame>>,
}

/// An activation's actual arguments and where its parameters currently live, from which a
/// `fn.arguments` read builds a fresh arguments object (see `bytecode::reflect`).
pub struct ReflectStash {
    /// The arguments the activation was called with.
    pub args: Rc<[Value]>,
    /// Tree-walker: the scope holding the parameter bindings.
    pub scope: Option<Env>,
    /// Compiled frame: its slot array (parameter `k` lives in slot `k`), valid while the frame
    /// is on the stack; null for a tree-walker frame.
    pub slots: *const Value,
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
