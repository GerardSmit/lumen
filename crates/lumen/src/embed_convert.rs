//! The JS host of `lumen-bind` ([`JsHost`]): every `#[lumen_bind::op]`, `#[class]` and
//! `#[module]` becomes a JS function, class or namespace object through it. Also the embedder
//! runtime around it: errors ([`OpError`]), promises ([`Deferred`], [`Promise`]), async work
//! ([`Completer`], [`AsyncHost`]) and class instances.
//!
//! # How a call works
//! [`JsHost::entry`] is a plain [`NativeFn`]. It builds an [`ArgCx`] over `(ctx, this, args)`,
//! the generated thunk converts each argument (borrowing where the JS storage allows), calls the
//! Rust fn and converts the result. The `ArgCx` owns everything a borrow needs to stay valid
//! (coerced strings, snapshot copies, `RefCell` guards of class instances, lent buffers) and
//! releases it when the call returns.
//!
//! # Byte-slice borrows (`&[u8]`, `&mut [u8]`)
//! A slice points straight into the `ArrayBuffer` backing store — no copy. The store lives in the
//! interpreter's buffer table, so the slice must not outlive any JS that could detach or resize
//! that buffer. Two mechanisms keep that sound:
//! * **Pointer borrow** (the fast path, ops without `&mut Ctx`): the op cannot run JS, so the
//!   table is untouched while the slices live.
//! * **Lending**: before anything that may run JS (a `&mut Ctx` handoff, a coercing conversion,
//!   an array walk), every borrowed buffer is *moved out* of the table into the `ArgCx`. Its bytes
//!   stay where they are (moving a `Vec` moves only its header), so outstanding slices remain
//!   valid; re-entrant JS sees the buffer as detached. The wrapper puts it back on return.
//!
//! Aliasing is checked per call: a `&mut [u8]` overlapping any other slice argument of the same
//! buffer is a `TypeError`. SharedArrayBuffer views are copied for `&[u8]` (racy memory cannot
//! back a shared Rust reference) and borrowed in place for `&mut [u8]`, which writes straight to
//! the shared memory; immutable buffers reject `&mut [u8]`.
//!
//! # Names
//! Free functions keep their Rust name; class members are camelCased (`set_x` sets `x`);
//! `name = ".."` / `rename(js = "..")` override both. A function's `length` is its number of
//! required parameters. Protocols: `iter` is `[Symbol.iterator]`, `next` returns
//! `{ value, done }`, `str` is `toString`, `len` is a `length` getter; the others are not
//! installed in JS.

use crate::fasthash::FastMap;
use crate::interpreter::{abrupt_value, Interp};
use crate::lstr::LStr;
use crate::value::{Callable, Gc, NativeFn, Object, Property, TaInfo, TaKind, Value, WeakGc};
pub use lumen_bind::Slot;
use lumen_bind::{
    flags, BigInt, Class, Elem, FnDesc, FnItem, FromArg, Host, IntKind, IntoError, IntoRet,
    Methods, Module, ModuleItems, Native, NextRet, Owner, Role, ScalarEntry, SpawnHost, State,
    StateHost,
};
use lumen_common::buffer::StoreSlot;
use lumen_common::native::NativeError;
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::rc::Rc;

/// The JS engine as a `lumen-bind` host.
pub struct JsHost;

/// A JS object handle that does not keep the object alive.
#[derive(Clone)]
pub struct WeakValue(WeakGc);

impl WeakValue {
    pub fn upgrade(&self) -> Option<Value> {
        self.0.upgrade().map(Value::Obj)
    }
}

/// The JS name of a bound fn (see the module docs).
pub fn js_name(d: &FnDesc) -> Cow<'static, str> {
    if let Some(n) = d.fixed_name("js") {
        return Cow::Borrowed(n);
    }
    match d.role {
        Role::Function => Cow::Borrowed(d.name),
        Role::Setter => Cow::Owned(lumen_bind::camel_case(lumen_bind::setter_property(d.name))),
        Role::Proto("str") => Cow::Borrowed("toString"),
        Role::Proto("len") => Cow::Borrowed("length"),
        _ => Cow::Owned(lumen_bind::camel_case(d.name)),
    }
}

/// The JS class name of `T`.
pub fn class_name<T: Class>() -> &'static str {
    T::DESC.name_for("js")
}

// =============================================================================================
// Conversion context
// =============================================================================================

#[derive(Clone, Copy)]
struct ByteBorrow {
    key: usize,
    base: *mut u8,
    off: usize,
    len: usize,
    mutable: bool,
    slot: Slot,
}

const INLINE_BORROWS: usize = 4;

#[derive(Default)]
pub(crate) struct Scratch {
    more_borrows: Vec<ByteBorrow>,
    lent: Vec<(usize, StoreSlot)>,
    strs: Vec<LStr>,
    vals: Vec<Box<[Value]>>,
    copies: Vec<Box<[u8]>>,
    guards: Vec<Box<dyn Any>>,
    new_target: Option<Value>,
}

const UNDEF: &Value = &Value::Undefined;

/// The per-call context of a bound fn ([`JsHost`]'s `Cx`). It owns whatever a borrowed
/// argument needs to stay valid until the op returns.
pub struct ArgCx<'s> {
    interp: *mut Interp,
    this: &'s Value,
    args: &'s [Value],
    desc: &'static FnDesc,
    /// Set while a `&mut Interp` derived from this context is live (see [`ArgCx::with_ctx`]).
    in_ctx: Cell<bool>,
    nborrow: Cell<usize>,
    borrows: UnsafeCell<[MaybeUninit<ByteBorrow>; INLINE_BORROWS]>,
    /// Lazily boxed side storage (`Box::into_raw`; null until first needed). A raw pointer
    /// keeps the drop glue of scratch-free calls down to one null test.
    scratch: Cell<*mut Scratch>,
}

impl<'s> ArgCx<'s> {
    #[inline]
    pub fn new(
        ctx: &'s mut Interp,
        this: &'s Value,
        args: &'s [Value],
        desc: &'static FnDesc,
    ) -> ArgCx<'s> {
        ArgCx {
            interp: ctx,
            this,
            args,
            desc,
            in_ctx: Cell::new(false),
            nborrow: Cell::new(0),
            borrows: UnsafeCell::new([const { MaybeUninit::uninit() }; INLINE_BORROWS]),
            scratch: Cell::new(std::ptr::null_mut()),
        }
    }

    /// Whether the op was declared `#[op(coerce)]` (JS ToNumber/ToString/ToBoolean semantics
    /// instead of strict type checks).
    #[inline]
    pub fn coerce(&self) -> bool {
        self.desc.flags & flags::COERCE != 0
    }

    /// The op's descriptor.
    pub fn desc(&self) -> &'static FnDesc {
        self.desc
    }

    /// All JS arguments, as passed.
    pub fn args(&self) -> &'s [Value] {
        self.args
    }

    /// The receiver, as passed.
    pub fn this(&self) -> &'s Value {
        self.this
    }

    #[inline]
    pub(crate) fn interp(&self) -> &Interp {
        assert!(!self.in_ctx.get(), "ArgCx used while its Ctx is lent out");
        // SAFETY: `interp` came from a `&mut Interp` that outlives `self`; no `&mut` derived
        // from it is live (checked above).
        unsafe { &*self.interp }
    }

    /// # Safety
    /// The returned reference must be dead before any other use of `self`'s interpreter.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn interp_mut(&self) -> &mut Interp {
        assert!(!self.in_ctx.get(), "ArgCx used while its Ctx is lent out");
        &mut *self.interp
    }

    fn scratch_ref(&self) -> Option<&Scratch> {
        // SAFETY: see `scratch`.
        unsafe { self.scratch.get().as_ref() }
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) fn scratch(&self) -> &mut Scratch {
        // SAFETY: single-threaded; every caller finishes with the `&mut Scratch` before calling
        // back into another method that takes it. Borrows handed out point into heap data owned
        // by scratch elements, never into the `Scratch` struct or its vectors' buffers.
        let mut p = self.scratch.get();
        if p.is_null() {
            p = Box::into_raw(Box::<Scratch>::default());
            self.scratch.set(p);
        }
        unsafe { &mut *p }
    }

    /// Run `f` with the interpreter (the `&mut Ctx` handoff). Buffers borrowed so far are lent
    /// first, so JS run by `f` cannot invalidate them.
    pub fn with_ctx<R>(&self, f: impl FnOnce(&mut Interp) -> R) -> R {
        self.before_js();
        // SAFETY: `in_ctx` makes every other interpreter access through `self` panic until `f`
        // returns, so this is the only live `&mut Interp`.
        let ctx = unsafe { self.interp_mut() };
        let reset = ResetOnDrop(&self.in_ctx);
        self.in_ctx.set(true);
        let r = f(ctx);
        drop(reset);
        r
    }

    /// Run `f` with the host state slot `T` (see [`State`]). Throws when the embedder never
    /// installed a `T` with `OpState::put`.
    pub fn with_state<T: 'static, R>(
        &self,
        f: impl FnOnce(&mut State<T>) -> R,
    ) -> Result<R, Value> {
        let ctx = unsafe { self.interp_mut() };
        let Some(slot) = ctx.host_state.get_mut::<T>() else {
            let msg = format!(
                "{}: host state `{}` is not installed (OpState::put)",
                self.label(),
                std::any::type_name::<T>()
            );
            return Err(ctx.make_error("Error", msg));
        };
        let reset = ResetOnDrop(&self.in_ctx);
        self.in_ctx.set(true);
        let st = State::from_mut(slot);
        let r = f(st);
        drop(reset);
        Ok(r)
    }

    /// Lend every buffer borrowed so far (see the module docs): called before anything that
    /// may run JS.
    #[inline]
    pub fn before_js(&self) {
        if self.nborrow.get() != 0 {
            self.lend_all();
        }
    }

    #[cold]
    fn lend_all(&self) {
        for i in 0..self.nborrow.get() {
            let key = self.borrow_at(i).key;
            self.lend(key);
        }
    }

    /// Move buffer `key` out of the interpreter's table (once); returns its base pointer.
    fn lend(&self, key: usize) -> Option<*mut u8> {
        let s = self.scratch();
        if let Some((_, b)) = s.lent.iter().find(|(k, _)| *k == key) {
            return Some(b.as_ptr());
        }
        let ctx = unsafe { self.interp_mut() };
        let buf = ctx.array_buffers.remove(&key)?;
        let p = buf.as_ptr();
        self.scratch().lent.push((key, buf));
        Some(p)
    }

    fn borrow_at(&self, i: usize) -> ByteBorrow {
        if i < INLINE_BORROWS {
            // SAFETY: slots below `nborrow` are initialized.
            unsafe { (*self.borrows.get())[i].assume_init() }
        } else {
            self.scratch().more_borrows[i - INLINE_BORROWS]
        }
    }

    fn push_borrow(&self, b: ByteBorrow) {
        let n = self.nborrow.get();
        if n < INLINE_BORROWS {
            unsafe { (*self.borrows.get())[n] = MaybeUninit::new(b) };
        } else {
            self.scratch().more_borrows.push(b);
        }
        self.nborrow.set(n + 1);
    }

    /// `resolve_view` with this call's lent buffers put back for the lookup. No JS runs while
    /// they are back; moving a store handle never moves its bytes, so earlier borrows stay valid.
    #[cold]
    fn resolve_with_lent(&self, v: &Value) -> Result<(usize, usize, usize), ViewErr> {
        let s = self.scratch();
        // SAFETY: no `&mut Interp` is live (asserted), and nothing below re-enters JS.
        let ctx = unsafe { self.interp_mut() };
        let keys: Vec<usize> = s.lent.iter().map(|(k, _)| *k).collect();
        for (key, buf) in s.lent.drain(..) {
            ctx.array_buffers.insert(key, buf);
        }
        let r = resolve_view(ctx, v);
        for key in keys {
            if let Some(buf) = ctx.array_buffers.remove(&key) {
                s.lent.push((key, buf));
            }
        }
        r
    }

    /// Borrow the bytes a view covers: `(pointer, length)`. The pointer is valid until the
    /// wrapper returns. This is the zero-copy path behind `&[u8]` / `&mut [u8]`.
    pub fn view_bytes(
        &self,
        v: &Value,
        at: Slot,
        mutable: bool,
    ) -> Result<(*mut u8, usize), Value> {
        let (key, off, len) = match resolve_view(self.interp(), v) {
            Ok(r) => r,
            Err(ViewErr::Detached) if self.scratch_ref().is_some_and(|s| !s.lent.is_empty()) => {
                // Possibly a buffer this call already lent out (a second view of it).
                self.resolve_with_lent(v)
                    .map_err(|e| self.view_error(e, at))?
            }
            Err(e) => return Err(self.view_error(e, at)),
        };
        let i = self.interp();
        if let Some(&id) = i.shared_buffers.get(&key) {
            if mutable {
                let mem = crate::interpreter::shared_mem_get(id)
                    .ok_or_else(|| self.type_error(at, "SharedArrayBuffer memory is gone"))?;
                let mut m = mem.lock().unwrap();
                let Some(window) = m.get_mut(off..off + len) else {
                    return Err(
                        self.type_error(at, "view is out of range of its SharedArrayBuffer")
                    );
                };
                return Ok((window.as_mut_ptr(), window.len()));
            }
            let copy: Box<[u8]> = crate::interpreter::shared_mem_get(id)
                .map(|m| {
                    let m = m.lock().unwrap();
                    m.get(off..off + len).unwrap_or_default().into()
                })
                .unwrap_or_default();
            let s = self.scratch();
            s.copies.push(copy);
            let c = s.copies.last_mut().unwrap();
            return Ok((c.as_mut_ptr(), c.len()));
        }
        // A buffer this call already lent out is absent from the table; ask its store directly.
        let lent_readonly = || {
            self.scratch_ref()
                .and_then(|s| {
                    s.lent
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, b)| b.is_readonly())
                })
                .unwrap_or(false)
        };
        if mutable && (i.buffer_immutable(key) || lent_readonly()) {
            return Err(self.type_error(at, "is backed by an immutable ArrayBuffer"));
        }
        // Alias check + base-pointer reuse against earlier borrows of the same buffer.
        let mut base = None;
        for k in 0..self.nborrow.get() {
            let b = self.borrow_at(k);
            if b.key != key {
                continue;
            }
            base = Some(b.base);
            let overlap = len != 0 && b.len != 0 && off < b.off + b.len && b.off < off + len;
            if overlap && (mutable || b.mutable) {
                let msg = format!(
                    "overlaps {} in the same buffer; a mutable byte slice cannot alias",
                    self.slot_label(b.slot)
                );
                return Err(self.type_error(at, &msg));
            }
        }
        let base = match base {
            Some(b) => b,
            None => {
                let lend_now = self.desc.flags & flags::CTX != 0
                    || self.scratch_ref().is_some_and(|s| !s.lent.is_empty());
                if lend_now {
                    match self.lend(key) {
                        Some(p) => p,
                        None => return Err(self.view_error(ViewErr::Detached, at)),
                    }
                } else {
                    let ctx = unsafe { self.interp_mut() };
                    match ctx.array_buffers.get(&key) {
                        Some(b) => b.as_ptr(),
                        None => return Err(self.view_error(ViewErr::Detached, at)),
                    }
                }
            }
        };
        self.push_borrow(ByteBorrow {
            key,
            base,
            off,
            len,
            mutable,
            slot: at,
        });
        // SAFETY: `off + len` is within the buffer (resolve_view checked the bounds).
        Ok((unsafe { base.add(off) }, len))
    }

    /// A copy of the bytes a view covers (the `Vec<u8>` argument path).
    pub fn view_copy(&self, v: &Value, at: Slot) -> Result<Vec<u8>, Value> {
        let (key, off, len) = match resolve_view(self.interp(), v) {
            Ok(r) => r,
            Err(ViewErr::Detached) if self.scratch_ref().is_some_and(|s| !s.lent.is_empty()) => {
                // Possibly a buffer this call already lent out (a second view of it).
                self.resolve_with_lent(v)
                    .map_err(|e| self.view_error(e, at))?
            }
            Err(e) => return Err(self.view_error(e, at)),
        };
        let i = self.interp();
        if let Some(&id) = i.shared_buffers.get(&key) {
            return Ok(crate::interpreter::shared_mem_get(id)
                .map(|m| {
                    m.lock()
                        .unwrap()
                        .get(off..off + len)
                        .unwrap_or_default()
                        .to_vec()
                })
                .unwrap_or_default());
        }
        if let Some(b) = i.array_buffers.get(&key) {
            return Ok(b.bytes()[off..off + len].to_vec());
        }
        // Lent by this very call: read the lent copy.
        if let Some(s) = self.scratch_ref() {
            if let Some((_, b)) = s.lent.iter().find(|(k, _)| *k == key) {
                return Ok(b.bytes()[off..off + len].to_vec());
            }
        }
        Err(self.view_error(ViewErr::Detached, at))
    }

    fn view_error(&self, e: ViewErr, at: Slot) -> Value {
        match e {
            ViewErr::NotView => self.type_error(
                at,
                "must be an ArrayBuffer, a TypedArray (Uint8Array, Buffer, ...) or a DataView",
            ),
            ViewErr::Detached => self.type_error(at, "is backed by a detached ArrayBuffer"),
        }
    }

    /// Keep `s` alive until the op returns and borrow it.
    fn hold_str(&self, s: LStr) -> &str {
        let sc = self.scratch();
        sc.strs.push(s);
        let p: *const str = sc.strs.last().unwrap().as_str();
        // SAFETY: an LStr's bytes live in its own heap block, which stays put while the LStr
        // (owned by scratch until the ArgCx drops) is alive.
        unsafe { &*p }
    }

    /// `"Class.op"` / `"op"`.
    pub fn label(&self) -> String {
        let name = js_name(self.desc);
        match self.desc.owner {
            Owner::Class(c) => format!("{}.{}", c.name_for("js"), name),
            _ => name.into_owned(),
        }
    }

    fn slot_label(&self, s: Slot) -> String {
        let mut out = match s.index() {
            None => "receiver (this)".to_string(),
            Some(i) => match self.desc.named().nth(i as usize) {
                Some(p) => format!("argument {} ({})", i + 1, p.name),
                None => format!("argument {}", i + 1),
            },
        };
        if let Some(e) = s.element() {
            out.push_str(&format!(" element {e}"));
        }
        out
    }

    /// A `TypeError: <op>: argument N (<name>) <what>` value.
    #[cold]
    pub fn type_error(&self, at: Slot, what: &str) -> Value {
        let msg = format!("{}: {} {}", self.label(), self.slot_label(at), what);
        self.interp().make_error("TypeError", msg)
    }

    /// A `RangeError: <op>: argument N (<name>) <what>` value.
    #[cold]
    pub fn range_error(&self, at: Slot, what: &str) -> Value {
        let msg = format!("{}: {} {}", self.label(), self.slot_label(at), what);
        self.interp().make_error("RangeError", msg)
    }

    #[cold]
    fn number_slow(&self, v: &Value, at: Slot) -> Result<f64, Value> {
        if self.coerce() {
            self.before_js();
            unsafe { self.interp_mut() }
                .to_number(v)
                .map_err(abrupt_value)
        } else {
            Err(self.type_error(at, "must be a number"))
        }
    }

    /// `#[op(async)]`: run `work` (the op body over its already-converted, owned arguments) on
    /// the host's worker pool; returns the promise (see [`Interp::spawn_blocking`]).
    pub fn spawn_blocking<R: IntoRet<JsHost> + Send + 'static>(
        &self,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<Value, Value> {
        self.before_js();
        // SAFETY: no borrow derived from the context is live past this call.
        let ctx = unsafe { self.interp_mut() };
        let p = ctx.spawn_blocking(work);
        p.into_ret(ctx)
    }

    /// Convert the op's result (the tail of every wrapper).
    #[inline(always)]
    pub fn ret<R: IntoRet<JsHost>>(&self, r: R) -> Result<Value, Value> {
        if R::MAY_RUN {
            self.before_js();
        }
        r.into_ret(unsafe { self.interp_mut() })
    }

    // ---- classes ----

    /// Finish a class constructor: wrap the Rust value in an instance whose prototype comes
    /// from `new.target` (so JS subclasses get their own prototype and still pass brand checks).
    fn construct<T: Class>(&self, value: T) -> Result<Value, Value> {
        self.before_js();
        let nt = self
            .scratch_ref()
            .and_then(|s| s.new_target.clone())
            .unwrap_or(Value::Undefined);
        let ctx = unsafe { self.interp_mut() };
        // `super(...)` from a JS subclass runs a native parent constructor with the derived
        // instance already allocated as `this`: attach the Rust value to it directly (returning
        // it unchanged, so the engine's native-parent graft has nothing to move).
        if let Value::Obj(o) = self.this {
            attach_instance(ctx, o, value);
            return Ok(self.this.clone());
        }
        let Some((_, class_proto)) = registered_class::<T>(ctx) else {
            return Err(unregistered::<T>(ctx));
        };
        let proto = match &nt {
            Value::Obj(_) => match ctx.get_member(&nt, "prototype").map_err(abrupt_value)? {
                Value::Obj(p) => p,
                _ => class_proto,
            },
            _ => class_proto,
        };
        Ok(new_instance(ctx, value, proto))
    }

    pub(crate) fn class_rc<T: Class>(&self, v: &Value, at: Slot) -> Result<Rc<RefCell<T>>, Value> {
        match host_data(self.interp(), v).and_then(|d| d.downcast::<RefCell<T>>().ok()) {
            Some(rc) => Ok(rc),
            None if at == Slot::THIS => {
                let msg = format!(
                    "{}: illegal invocation (receiver is not a {})",
                    self.label(),
                    class_name::<T>()
                );
                Err(self.interp().make_error("TypeError", msg))
            }
            None => Err(self.type_error(at, &format!("must be a {}", class_name::<T>()))),
        }
    }

    /// Borrow a class instance shared (`&self` receivers, `&T` arguments).
    pub fn class_ref<'a, T: Class>(&'a self, v: &Value, at: Slot) -> Result<&'a T, Value> {
        if let Some(entry) = host_entry(self.interp(), v) {
            if let Ok((ptr, guard)) = (entry.view_ref)(&entry.data, TypeId::of::<T>()) {
                // The erased guard retains the allocation and its shared borrow.
                let value = unsafe { &*ptr }.downcast_ref::<T>().expect("matching class view");
                self.scratch().guards.push(guard);
                return Ok(value);
            }
        }
        let rc = self.class_rc::<T>(v, at)?;
        let r = match rc.try_borrow() {
            Ok(r) => r,
            Err(_) => return Err(self.borrow_conflict::<T>(at)),
        };
        // SAFETY: the guard keeps `rc` (and so the RefCell) alive, and drops the `Ref` first.
        let r: std::cell::Ref<'static, T> = unsafe { std::mem::transmute(r) };
        let p: *const T = &*r;
        self.scratch().guards.push(Box::new(RefGuard {
            g: Some(r),
            _rc: rc,
        }));
        Ok(unsafe { &*p })
    }

    /// Borrow a class instance exclusively (`&mut self` receivers, `&mut T` arguments). A
    /// conflicting borrow (re-entrant call on the same instance) throws a TypeError.
    #[allow(clippy::mut_from_ref)]
    pub fn class_mut<'a, T: Class>(&'a self, v: &Value, at: Slot) -> Result<&'a mut T, Value> {
        if let Some(entry) = host_entry(self.interp(), v) {
            if let Ok((ptr, guard)) = (entry.view_mut)(&entry.data, TypeId::of::<T>()) {
                let value = unsafe { &mut *ptr }.downcast_mut::<T>().expect("matching class view");
                self.scratch().guards.push(guard);
                return Ok(value);
            }
        }
        let rc = self.class_rc::<T>(v, at)?;
        let r = match rc.try_borrow_mut() {
            Ok(r) => r,
            Err(_) => return Err(self.borrow_conflict::<T>(at)),
        };
        // SAFETY: as in `class_ref`.
        let mut r: std::cell::RefMut<'static, T> = unsafe { std::mem::transmute(r) };
        let p: *mut T = &mut *r;
        self.scratch().guards.push(Box::new(MutGuard {
            g: Some(r),
            _rc: rc,
        }));
        Ok(unsafe { &mut *p })
    }

    #[cold]
    fn borrow_conflict<T: Class>(&self, at: Slot) -> Value {
        self.type_error(
            at,
            &format!(
                "is a {} already in use by an enclosing call (re-entrant borrow)",
                class_name::<T>()
            ),
        )
    }
}

struct ResetOnDrop<'a>(&'a Cell<bool>);
impl Drop for ResetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

struct RefGuard<T: 'static> {
    g: Option<std::cell::Ref<'static, T>>,
    _rc: Rc<RefCell<T>>,
}
struct MutGuard<T: 'static> {
    g: Option<std::cell::RefMut<'static, T>>,
    _rc: Rc<RefCell<T>>,
}
impl<T> Drop for RefGuard<T> {
    fn drop(&mut self) {
        self.g.take();
    }
}
impl<T> Drop for MutGuard<T> {
    fn drop(&mut self) {
        self.g.take();
    }
}

impl Drop for ArgCx<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        // Fast path: plain-value ops never allocate scratch.
        if !self.scratch.get().is_null() {
            self.drop_scratch();
        }
    }
}

impl ArgCx<'_> {
    #[cold]
    #[inline(never)]
    fn drop_scratch(&mut self) {
        let p = self.scratch.replace(std::ptr::null_mut());
        if !p.is_null() {
            // SAFETY: `p` came from `Box::into_raw` in `scratch()` and is released only here.
            let mut s = unsafe { Box::from_raw(p) };
            s.guards.clear();
            if !s.lent.is_empty() {
                // SAFETY: the wrapper is returning; nothing borrows from the context any more.
                let ctx = unsafe { &mut *self.interp };
                for (key, buf) in s.lent.drain(..) {
                    // Absent = still lent by us. (Present would mean JS created a new buffer at
                    // that address, impossible while the argument keeps the old one alive.)
                    ctx.array_buffers.entry(key).or_insert(buf);
                }
            }
        }
    }
}

enum ViewErr {
    NotView,
    Detached,
}

/// `(buffer key, byte offset, byte length)` of an ArrayBuffer / TypedArray / DataView.
fn resolve_view(i: &Interp, v: &Value) -> Result<(usize, usize, usize), ViewErr> {
    let Value::Obj(o) = v else {
        return Err(ViewErr::NotView);
    };
    let p = Gc::as_ptr(o) as usize;
    if let Some(info) = i.typed_arrays.get(&p) {
        let info: TaInfo = *info;
        let len = i.ta_len(&info).ok_or(ViewErr::Detached)?;
        return Ok((info.buffer, info.offset, len * info.kind.elsize()));
    }
    if let Some(&(b, off, len, track)) = i.data_views.get(&p) {
        let buflen = i.array_buffers.get(&b).ok_or(ViewErr::Detached)?.len();
        return if track {
            if off > buflen {
                Err(ViewErr::Detached)
            } else {
                Ok((b, off, buflen - off))
            }
        } else if off + len > buflen {
            Err(ViewErr::Detached)
        } else {
            Ok((b, off, len))
        };
    }
    if let Some(b) = i.array_buffers.get(&p) {
        return Ok((p, 0, b.len()));
    }
    if o.borrow().props.contains("\u{0}ab_max_byte_length") {
        return Err(ViewErr::Detached);
    }
    Err(ViewErr::NotView)
}

// =============================================================================================
// The host
// =============================================================================================

#[inline(always)]
fn js_entry<N: Native<JsHost>>(
    ctx: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if matches!(N::DESC.role, Role::Constructor) {
        return js_construct::<N>(ctx, this, args);
    }
    let cx = ArgCx::new(ctx, &this, args, N::DESC);
    let result = N::call(&cx);
    drop(cx);
    flush_deferred_microtasks(ctx);
    result
}

#[inline(never)]
fn js_construct<N: Native<JsHost>>(
    ctx: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if !ctx.constructing {
        let name = match N::DESC.owner {
            Owner::Class(c) => c.name_for("js"),
            _ => N::DESC.name,
        };
        let msg = format!("Class constructor {name} cannot be invoked without 'new'");
        return Err(ctx.make_error("TypeError", msg));
    }
    let nt = ctx.new_target.clone();
    let cx = ArgCx::new(ctx, &this, args, N::DESC);
    cx.scratch().new_target = Some(nt);
    let result = N::call(&cx);
    drop(cx);
    flush_deferred_microtasks(ctx);
    result
}

impl ArgCx<'_> {
    /// A 64-bit integer argument: a Number that is a safe integer (|n| <= 2^53-1), or a BigInt.
    fn int64(&self, v: &Value, at: Slot, min: i128, max: i128) -> Result<i128, Value> {
        let n: i128 = match v {
            Value::BigInt(b) => b.to_i128_wrapping(),
            Value::Num(n) => self.safe_int(*n, at)?,
            _ if self.coerce() => {
                let n = self.number_slow(v, at)?;
                self.safe_int(n, at)?
            }
            _ => return Err(self.type_error(at, "must be an integer (number or bigint)")),
        };
        if n < min || n > max {
            return Err(self.range_error(at, "is out of range"));
        }
        Ok(n)
    }

    fn safe_int(&self, n: f64, at: Slot) -> Result<i128, Value> {
        if n.fract() == 0.0 && n.abs() <= 9007199254740991.0 {
            Ok(n as i128)
        } else {
            Err(self.range_error(at, "must be a safe integer"))
        }
    }

    /// The elements of a JS Array (reads go through `[[Get]]`, so getters run), held until the
    /// call returns.
    fn array_items(&self, v: &Value, at: Slot) -> Result<&[Value], Value> {
        let is_array = matches!(v, Value::Obj(o) if o.borrow().exotic.is_array());
        if !is_array {
            return Err(self.type_error(at, "must be an array"));
        }
        self.before_js();
        let items: Box<[Value]> = {
            let ctx = unsafe { self.interp_mut() };
            let len = ctx.get_member(v, "length").map_err(abrupt_value)?;
            let len = match len {
                Value::Num(n) if n >= 0.0 => n as usize,
                _ => 0,
            };
            let mut items = Vec::with_capacity(len.min(1 << 16));
            for k in 0..len {
                items.push(ctx.get_member(v, &k.to_string()).map_err(abrupt_value)?);
            }
            items.into_boxed_slice()
        };
        let s = self.scratch();
        s.vals.push(items);
        let p: *const [Value] = &**s.vals.last().unwrap();
        // SAFETY: the boxed slice is owned by scratch until the context drops.
        Ok(unsafe { &*p })
    }
}

impl Host for JsHost {
    const NAME: &'static str = "js";
    type Value = Value;
    type Error = Value;
    type Ctx = Interp;
    type Cx<'s> = ArgCx<'s>;
    type Entry = NativeFn;

    fn entry<N: Native<Self>>() -> NativeFn {
        js_entry::<N>
    }

    /// JS has no keywords: named parameters are the arguments in declaration order, and
    /// `undefined` is a missing argument (so it takes the default).
    #[inline(always)]
    fn bind<'c, const N: usize>(cx: &'c ArgCx<'_>) -> Result<[Option<&'c Value>; N], Value> {
        let args: &'c [Value] = cx.args;
        let max = cx.desc.max_pos as usize;
        // Keyword-only parameters after `*args` cannot be passed positionally.
        let positional = if N > max && cx.desc.has_varargs() {
            max
        } else {
            N
        };
        Ok(std::array::from_fn(|i| match args.get(i) {
            Some(Value::Undefined) | None => None,
            Some(v) if i < positional => Some(v),
            Some(_) => None,
        }))
    }

    fn rest<'c>(cx: &'c ArgCx<'_>) -> &'c [Value] {
        let args: &'c [Value] = cx.args;
        args.get(cx.desc.max_pos as usize..).unwrap_or(&[])
    }

    fn varkw<'c>(_: &'c ArgCx<'_>) -> Vec<(&'c str, &'c Value)> {
        Vec::new()
    }

    #[inline(always)]
    fn this<'c>(cx: &'c ArgCx<'_>) -> &'c Value {
        cx.this
    }

    #[inline(always)]
    fn absent() -> &'static Value {
        UNDEF
    }

    #[inline(always)]
    fn with_ctx<R>(cx: &ArgCx<'_>, f: impl FnOnce(&mut Interp) -> R) -> R {
        cx.with_ctx(f)
    }

    #[inline(always)]
    fn ret<R: IntoRet<Self>>(cx: &ArgCx<'_>, r: R) -> Result<Value, Value> {
        cx.ret(r)
    }

    fn ret_next<R: NextRet<Self>>(cx: &ArgCx<'_>, r: R) -> Result<Value, Value> {
        cx.before_js();
        r.into_next(unsafe { cx.interp_mut() })
    }

    fn construct<T: Class>(cx: &ArgCx<'_>, value: T) -> Result<Value, Value> {
        cx.construct(value)
    }

    #[inline(always)]
    fn is_none(v: &Value) -> bool {
        matches!(v, Value::Undefined | Value::Null)
    }

    #[inline(always)]
    fn to_f64(cx: &ArgCx<'_>, v: &Value, at: Slot) -> Result<f64, Value> {
        match v {
            Value::Num(n) => Ok(*n),
            _ => cx.number_slow(v, at),
        }
    }

    /// Up to 32 bits: ToInt32 / ToUint32 wrapping (modulo 2^N) of a Number, like WebIDL integer
    /// types. 64 bits: a safe-integer Number or a BigInt in range.
    #[inline(always)]
    fn to_int(cx: &ArgCx<'_>, v: &Value, at: Slot, kind: IntKind) -> Result<i128, Value> {
        if kind.bits <= 32 {
            let n = Self::to_f64(cx, v, at)?;
            Ok(crate::eval::to_int32(n) as i128)
        } else {
            cx.int64(v, at, kind.min(), kind.max())
        }
    }

    fn to_bigint(cx: &ArgCx<'_>, v: &Value, at: Slot) -> Result<BigInt, Value> {
        match v {
            Value::BigInt(b) => Ok(b.clone()),
            Value::Num(n) => cx.safe_int(*n, at).map(BigInt::from_i128),
            _ => Err(cx.type_error(at, "must be a bigint")),
        }
    }

    #[inline]
    fn to_bool(cx: &ArgCx<'_>, v: &Value, at: Slot) -> Result<bool, Value> {
        match v {
            Value::Bool(b) => Ok(*b),
            _ if cx.coerce() => Ok(cx.interp().to_boolean(v)),
            _ => Err(cx.type_error(at, "must be a boolean")),
        }
    }

    /// Borrows the JS string's UTF-8 storage. Lone surrogates appear as the private-use
    /// scalars U+10F800.. (lumen's internal smuggling scheme), so the `str` is always valid.
    #[inline]
    fn to_str<'c>(cx: &'c ArgCx<'_>, v: &'c Value, at: Slot) -> Result<&'c str, Value> {
        match v {
            Value::Str(s) => Ok(s.as_str()),
            _ if cx.coerce() => {
                cx.before_js();
                let s = unsafe { cx.interp_mut() }
                    .to_string(v)
                    .map_err(abrupt_value)?;
                Ok(cx.hold_str(s))
            }
            _ => Err(cx.type_error(at, "must be a string")),
        }
    }

    /// Zero-copy view of an ArrayBuffer / TypedArray (any element type) / DataView.
    #[inline]
    fn to_bytes<'c>(cx: &'c ArgCx<'_>, v: &'c Value, at: Slot) -> Result<&'c [u8], Value> {
        let (p, n) = cx.view_bytes(v, at, false)?;
        // SAFETY: see the module docs (pointer borrow / lending).
        Ok(unsafe { std::slice::from_raw_parts(p, n) })
    }

    /// Zero-copy mutable view; writes are visible to JS after the call.
    #[inline]
    fn to_bytes_mut<'c>(cx: &'c ArgCx<'_>, v: &'c Value, at: Slot) -> Result<&'c mut [u8], Value> {
        let (p, n) = cx.view_bytes(v, at, true)?;
        // SAFETY: as above, plus the per-call alias check.
        Ok(unsafe { std::slice::from_raw_parts_mut(p, n) })
    }

    fn to_byte_vec(cx: &ArgCx<'_>, v: &Value, at: Slot) -> Result<Vec<u8>, Value> {
        cx.view_copy(v, at)
    }

    fn to_seq<'c>(cx: &'c ArgCx<'_>, v: &'c Value, at: Slot) -> Result<&'c [Value], Value> {
        cx.array_items(v, at)
    }

    fn class_ref<'c, T: Class>(cx: &'c ArgCx<'_>, v: &'c Value, at: Slot) -> Result<&'c T, Value> {
        cx.class_ref(v, at)
    }

    fn class_mut<'c, T: Class>(
        cx: &'c ArgCx<'_>,
        v: &'c Value,
        at: Slot,
    ) -> Result<&'c mut T, Value> {
        cx.class_mut(v, at)
    }

    #[inline(always)]
    fn unit(_: &mut Interp) -> Value {
        Value::Undefined
    }

    #[inline(always)]
    fn none(_: &mut Interp) -> Value {
        Value::Null
    }

    #[inline(always)]
    fn from_bool(_: &mut Interp, b: bool) -> Value {
        Value::Bool(b)
    }

    #[inline(always)]
    fn from_f64(_: &mut Interp, x: f64) -> Value {
        Value::Num(x)
    }

    /// A Number; beyond ±(2^53-1) a RangeError (return `BigInt` / `BigI64` for a BigInt).
    #[inline(always)]
    fn from_int(ctx: &mut Interp, n: i128) -> Result<Value, Value> {
        if n.abs() <= 9007199254740991 {
            Ok(Value::Num(n as f64))
        } else {
            Err(ctx.make_error(
                "RangeError",
                format!("result {n} exceeds Number.MAX_SAFE_INTEGER (return a BigInt)"),
            ))
        }
    }

    fn from_bigint(_: &mut Interp, n: BigInt) -> Value {
        Value::BigInt(n)
    }

    #[inline]
    fn from_str(_: &mut Interp, s: &str) -> Value {
        Value::str(s)
    }

    #[inline]
    fn from_string(_: &mut Interp, s: String) -> Value {
        Value::from_string(s)
    }

    /// A `Uint8Array` that adopts the vector as its backing store (no copy).
    fn from_bytes(ctx: &mut Interp, b: Vec<u8>) -> Value {
        uint8array_from_vec(ctx, b)
    }

    fn from_list(ctx: &mut Interp, items: Vec<Value>) -> Value {
        ctx.make_array(items)
    }

    fn from_tuple(ctx: &mut Interp, items: Vec<Value>) -> Value {
        ctx.make_array(items)
    }

    fn new_instance<T: Class>(ctx: &mut Interp, value: T) -> Result<Value, Value> {
        match registered_class::<T>(ctx) {
            Some((_, proto)) => Ok(new_instance(ctx, value, proto)),
            None => Err(unregistered::<T>(ctx)),
        }
    }

    /// `{ value, done }`.
    fn iter_step(ctx: &mut Interp, item: Option<Value>) -> Result<Value, Value> {
        let o = ctx.new_object();
        {
            let mut b = o.borrow_mut();
            let done = item.is_none();
            b.props
                .insert("value", Property::plain(item.unwrap_or(Value::Undefined)));
            b.props.insert("done", Property::plain(Value::Bool(done)));
        }
        Ok(Value::Obj(o))
    }

    fn error(ctx: &mut Interp, e: NativeError) -> Value {
        OpError::from(e).to_value(ctx)
    }

    fn class_object<T: Methods<Self>>(ctx: &mut Interp) -> Result<Value, Value> {
        Ok(class_entry::<T>(ctx).0)
    }

    fn module_object<M: Module<Self>>(ctx: &mut Interp) -> Result<Value, Value> {
        let ns = ctx.new_object();
        install_items(ctx, &ns, ModuleItems::of::<M>())?;
        Ok(Value::Obj(ns))
    }
}

impl StateHost for JsHost {
    fn with_state<T: 'static, R>(
        cx: &ArgCx<'_>,
        f: impl FnOnce(&mut State<T>) -> R,
    ) -> Result<R, Value> {
        cx.with_state(f)
    }
}

impl SpawnHost for JsHost {
    fn spawn_blocking<R: IntoRet<Self> + Send + 'static>(
        cx: &ArgCx<'_>,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<Value, Value> {
        cx.spawn_blocking(work)
    }

    /// The promise from `spawn_blocking` passes through; an argument conversion error (thrown
    /// before any work was spawned) becomes a rejected promise.
    fn async_ret(cx: &ArgCx<'_>, r: Result<Value, Value>) -> Result<Value, Value> {
        let e = match r {
            Ok(p) => return Ok(p),
            Err(e) => e,
        };
        cx.with_ctx(|ctx| {
            let d = Deferred::new(ctx);
            let p = d.promise();
            ctx.reject_promise(&p, e);
            Ok(p)
        })
    }
}

/// Install a module's items on `target`.
fn install_items(ctx: &mut Interp, target: &Gc, items: ModuleItems<JsHost>) -> Result<(), Value> {
    for f in items.functions.iter().filter(|f| f.desc.exposed_to("js")) {
        let v = ctx.bound_function(f);
        target
            .borrow_mut()
            .props
            .insert(&*js_name(f.desc), Property::builtin(v));
    }
    for c in items.classes.iter().filter(|c| c.desc.exposed_to("js")) {
        let v = (c.object)(ctx)?;
        target
            .borrow_mut()
            .props
            .insert(c.desc.name_for("js"), Property::builtin(v));
    }
    for k in &items.constants {
        let v = (k.value)(ctx)?;
        target
            .borrow_mut()
            .props
            .insert(k.name, Property::builtin(v));
    }
    if let Some(init) = items.init {
        init(ctx, &Value::Obj(target.clone()))?;
    }
    Ok(())
}

// ---- JS-only parameter and result types ------------------------------------------------------

impl<'a> FromArg<'a, JsHost> for Value {
    #[inline(always)]
    fn from_arg(_: &'a ArgCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Value> {
        Ok(v.clone())
    }
}

impl<'a> FromArg<'a, JsHost> for &'a Value {
    #[inline(always)]
    fn from_arg(_: &'a ArgCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Value> {
        Ok(v)
    }
}

/// A BigInt-typed 64-bit integer: accepts a BigInt (or an integral Number) and always returns
/// a BigInt. Plain `i64`/`u64` return Numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BigI64(pub i64);
/// The unsigned counterpart of [`BigI64`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BigU64(pub u64);

impl<'a> FromArg<'a, JsHost> for BigI64 {
    fn from_arg(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.int64(v, at, i64::MIN as i128, i64::MAX as i128)
            .map(|n| BigI64(n as i64))
    }
}
impl<'a> FromArg<'a, JsHost> for BigU64 {
    fn from_arg(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.int64(v, at, 0, u64::MAX as i128)
            .map(|n| BigU64(n as u64))
    }
}

/// Any JS object (functions included).
#[derive(Clone)]
pub struct JsObject(Value);
/// A callable JS value.
#[derive(Clone)]
pub struct JsFunction(Value);

impl JsObject {
    pub fn value(&self) -> &Value {
        &self.0
    }
    pub fn into_value(self) -> Value {
        self.0
    }
    /// `obj[key]` (runs getters).
    pub fn get(&self, ctx: &mut Interp, key: &str) -> Result<Value, OpError> {
        ctx.get_member(&self.0, key)
            .map_err(|a| OpError::thrown(abrupt_value(a)))
    }
    pub fn from_value(v: Value) -> Option<JsObject> {
        matches!(v, Value::Obj(_)).then_some(JsObject(v))
    }
}

impl JsFunction {
    pub fn value(&self) -> &Value {
        &self.0
    }
    pub fn into_value(self) -> Value {
        self.0
    }
    /// Call it; a JS exception comes back as `OpError::thrown`, so `?` rethrows it unchanged.
    pub fn call(&self, ctx: &mut Interp, this: Value, args: &[Value]) -> Result<Value, OpError> {
        ctx.invoke(self.0.clone(), this, args)
            .map_err(OpError::thrown)
    }
    pub fn from_value(v: Value) -> Option<JsFunction> {
        v.is_callable().then_some(JsFunction(v))
    }
}

impl<'a> FromArg<'a, JsHost> for JsObject {
    fn from_arg(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        match v {
            Value::Obj(_) => Ok(JsObject(v.clone())),
            _ => Err(cx.type_error(at, "must be an object")),
        }
    }
}

impl<'a> FromArg<'a, JsHost> for JsFunction {
    fn from_arg(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        if v.is_callable() {
            Ok(JsFunction(v.clone()))
        } else {
            Err(cx.type_error(at, "must be a function"))
        }
    }
}

impl<'a, T: Class> FromArg<'a, JsHost> for Rc<RefCell<T>> {
    /// A shared handle to a class instance's Rust value (no borrow held during the call).
    fn from_arg(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.class_rc::<T>(v, at)
    }
}

macro_rules! plain_ret {
    ($($t:ty => |$s:ident, $c:ident| $e:expr;)*) => {$(
        impl IntoRet<JsHost> for $t {
            const MAY_RUN: bool = false;
            #[inline(always)]
            fn into_ret(self, $c: &mut Interp) -> Result<Value, Value> {
                let $s = self;
                let _ = &$c;
                Ok($e)
            }
        }
    )*};
}

plain_ret! {
    Value => |s, _c| s;
    JsObject => |s, _c| s.0;
    JsFunction => |s, _c| s.0;
    BigI64 => |s, _c| Value::bigint_from_i64(s.0);
    BigU64 => |s, _c| Value::bigint_from_u64(s.0);
    JsArrayBuffer => |s, c| c.make_array_buffer_from(s.0);
}

impl IntoRet<JsHost> for &Value {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(self.clone())
    }
}

/// Return an `ArrayBuffer` (instead of a `Uint8Array`) adopting the bytes.
pub struct JsArrayBuffer(pub Vec<u8>);

impl Elem for Value {}
impl Elem for JsObject {}
impl Elem for JsFunction {}
impl Elem for BigI64 {}
impl Elem for BigU64 {}

/// A fresh `Uint8Array` over a fixed-length `ArrayBuffer` that owns `bytes` — built directly
/// (like `TypedArray` construction does internally) so no JS-visible lookup or copy happens.
fn uint8array_from_vec(i: &mut Interp, bytes: Vec<u8>) -> Value {
    let len = bytes.len();
    let buf = i.make_array_buffer_from(bytes);
    let Value::Obj(b) = &buf else { unreachable!() };
    let buf_ptr = Gc::as_ptr(b) as usize;
    let obj = Object::new(i.extra_protos.get("Uint8Array").cloned());
    let p = Gc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
    obj.borrow().ic_plain.set(false);
    i.typed_arrays.insert(
        p,
        TaInfo {
            buffer: buf_ptr,
            offset: 0,
            len,
            kind: TaKind::U8,
            track: false,
        },
    );
    i.ta_buffer.insert(p, buf);
    Value::Obj(obj)
}
// =============================================================================================
// Errors
// =============================================================================================

enum OpErrorRepr {
    New {
        class: &'static str,
        message: Cow<'static, str>,
        code: Option<Cow<'static, str>>,
    },
    Thrown(Value),
}

/// An op failure: either a new JS error (`class` + message, optional Node-style `code`) or an
/// already-thrown JS value being propagated (`OpError::thrown`, or `?` on a `Result<_, Value>`).
pub struct OpError(Box<OpErrorRepr>);

/// `Result<T, OpError>`.
pub type OpResult<T> = Result<T, OpError>;

impl OpError {
    /// A new `class` error (`"Error"`, `"TypeError"`, `"RangeError"`, ...).
    pub fn new(class: &'static str, message: impl Into<Cow<'static, str>>) -> OpError {
        OpError(Box::new(OpErrorRepr::New {
            class,
            message: message.into(),
            code: None,
        }))
    }
    pub fn error(message: impl Into<Cow<'static, str>>) -> OpError {
        OpError::new("Error", message)
    }
    pub fn type_error(message: impl Into<Cow<'static, str>>) -> OpError {
        OpError::new("TypeError", message)
    }
    pub fn range_error(message: impl Into<Cow<'static, str>>) -> OpError {
        OpError::new("RangeError", message)
    }
    pub fn syntax_error(message: impl Into<Cow<'static, str>>) -> OpError {
        OpError::new("SyntaxError", message)
    }
    /// Rethrow a JS value unchanged.
    pub fn thrown(v: Value) -> OpError {
        OpError(Box::new(OpErrorRepr::Thrown(v)))
    }
    /// Attach a Node-style `err.code` (`"ENOENT"`, `"ERR_INVALID_ARG_TYPE"`, ...).
    pub fn with_code(mut self, c: impl Into<Cow<'static, str>>) -> OpError {
        if let OpErrorRepr::New { code, .. } = &mut *self.0 {
            *code = Some(c.into());
        }
        self
    }
    /// The error class (`"Thrown"` for a propagated JS value).
    pub fn class(&self) -> &str {
        match &*self.0 {
            OpErrorRepr::New { class, .. } => class,
            OpErrorRepr::Thrown(_) => "Thrown",
        }
    }
    /// The message (empty for a propagated JS value).
    pub fn message(&self) -> &str {
        match &*self.0 {
            OpErrorRepr::New { message, .. } => message,
            OpErrorRepr::Thrown(_) => "",
        }
    }
    /// The JS value to throw.
    pub fn to_value(self, ctx: &mut Interp) -> Value {
        match *self.0 {
            OpErrorRepr::Thrown(v) => v,
            OpErrorRepr::New {
                class,
                message,
                code,
            } => {
                let message = message.into_owned();
                let dom = matches!(class, "InvalidStateError" | "NotFoundError" | "HierarchyRequestError" | "QuotaExceededError" | "NotSupportedError" | "WrongDocumentError" | "InvalidCharacterError" | "NoModificationAllowedError" | "NamespaceError");
                let e = if dom {
                    let global = ctx.global_object();
                    match ctx.get_member(&global, "DOMException") {
                        Ok(constructor) if constructor.is_callable() => ctx.construct_value(constructor, &[Value::str(&message), Value::str(class)]).unwrap_or_else(|error| error),
                        _ => ctx.make_error(class, message),
                    }
                } else { ctx.make_error(class, message) };
                if let (Some(code), Value::Obj(o)) = (code, &e) {
                    o.borrow_mut()
                        .props
                        .insert("code", Property::plain(Value::str(&*code)));
                }
                e
            }
        }
    }
}

struct DeferredMicrotasks(Rc<RefCell<Vec<Value>>>);

pub(crate) fn flush_deferred_microtasks(ctx: &mut Interp) {
    let pending = ctx.host_state.get::<DeferredMicrotasks>().map(|queue| std::mem::take(&mut *queue.0.borrow_mut())).unwrap_or_default();
    for callback in pending { ctx.queue_microtask(callback); }
}

impl Interp {
    /// Native mutation sinks can enqueue jobs without borrowing the interpreter.
    pub fn deferred_microtasks(&mut self) -> Rc<RefCell<Vec<Value>>> {
        if !self.host_state.has::<DeferredMicrotasks>() { self.host_state.put(DeferredMicrotasks(Rc::new(RefCell::new(Vec::new())))); }
        self.host_state.get::<DeferredMicrotasks>().unwrap().0.clone()
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            OpErrorRepr::New { class, message, .. } => write!(f, "{class}: {message}"),
            OpErrorRepr::Thrown(_) => f.write_str("<thrown JS value>"),
        }
    }
}
impl std::fmt::Debug for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl From<Value> for OpError {
    fn from(v: Value) -> OpError {
        OpError::thrown(v)
    }
}
impl From<String> for OpError {
    fn from(s: String) -> OpError {
        OpError::error(s)
    }
}
impl From<&str> for OpError {
    fn from(s: &str) -> OpError {
        OpError::error(s.to_owned())
    }
}
impl From<std::io::Error> for OpError {
    /// An `Error` with a Node-style `code` (`ENOENT`, `EACCES`, ...) when the kind maps to one.
    fn from(e: std::io::Error) -> OpError {
        NativeError::from(e).into()
    }
}
macro_rules! op_error_from {
    ($($t:ty => $class:literal),* $(,)?) => {$(
        impl From<$t> for OpError {
            fn from(e: $t) -> OpError {
                OpError::new($class, e.to_string())
            }
        }
    )*};
}
op_error_from!(
    std::fmt::Error => "Error",
    std::num::ParseIntError => "SyntaxError",
    std::num::ParseFloatError => "SyntaxError",
    std::num::TryFromIntError => "RangeError",
    std::str::Utf8Error => "TypeError",
    std::string::FromUtf8Error => "TypeError",
    Box<dyn std::error::Error + Send + Sync> => "Error",
);

macro_rules! into_error_via_op_error {
    ($($t:ty),* $(,)?) => {$(
        impl IntoError<JsHost> for $t {
            fn into_error(self, ctx: &mut Interp) -> Value {
                OpError::from(self).to_value(ctx)
            }
        }
    )*};
}
into_error_via_op_error!(
    OpError,
    Value,
    SendError,
    std::fmt::Error,
    std::num::ParseIntError,
    std::num::ParseFloatError,
    std::num::TryFromIntError,
    std::str::Utf8Error,
    std::string::FromUtf8Error,
    Box<dyn std::error::Error + Send + Sync>,
);

// =============================================================================================
// Promises
// =============================================================================================

/// A pending promise the host settles later (from its event loop, a completion callback, ...).
/// Not `Send`: settle it on the engine's thread. Dropping it unsettled leaves the promise pending
/// forever.
pub struct Deferred {
    promise: Value,
}

impl Deferred {
    pub fn new(ctx: &mut Interp) -> Deferred {
        Deferred {
            promise: ctx.new_promise(),
        }
    }
    /// The promise JS sees.
    pub fn promise(&self) -> Value {
        self.promise.clone()
    }
    /// Fulfil with `v` (a conversion error rejects instead). Queues reactions as microtasks.
    pub fn resolve<T: IntoRet<JsHost>>(self, ctx: &mut Interp, v: T) {
        match v.into_ret(ctx) {
            Ok(v) => ctx.resolve_promise(&self.promise, v),
            Err(e) => ctx.reject_promise(&self.promise, e),
        }
    }
    pub fn reject(self, ctx: &mut Interp, e: impl Into<OpError>) {
        let v = e.into().to_value(ctx);
        ctx.reject_promise(&self.promise, v);
    }
    /// The standard `(resolve, reject)` JS function pair — what a callback-based host API
    /// expects (e.g. `lumen_host::TaskRegistry::register(resolve, Some(reject), decode)`).
    pub fn resolving_functions(&self, ctx: &mut Interp) -> (Value, Value) {
        ctx.make_resolver_pair(&self.promise)
    }
}

enum PromiseRepr<T> {
    Ready(Result<T, OpError>),
    Pending(Value),
}

/// A promise result: `fn text(&self) -> Promise<String>`. Either settled now
/// ([`Promise::resolved`] / [`Promise::rejected`] / [`Promise::ready`]) or tied to a
/// [`Deferred`] the host settles later ([`Promise::pending`]).
pub struct Promise<T>(PromiseRepr<T>);

impl<T> Promise<T> {
    pub fn resolved(v: T) -> Promise<T> {
        Promise(PromiseRepr::Ready(Ok(v)))
    }
    pub fn rejected(e: impl Into<OpError>) -> Promise<T> {
        Promise(PromiseRepr::Ready(Err(e.into())))
    }
    pub fn ready<E: Into<OpError>>(r: Result<T, E>) -> Promise<T> {
        Promise(PromiseRepr::Ready(r.map_err(Into::into)))
    }
    pub fn pending(d: &Deferred) -> Promise<T> {
        Promise(PromiseRepr::Pending(d.promise()))
    }
}

impl<T: IntoRet<JsHost>> IntoRet<JsHost> for Promise<T> {
    fn into_ret(self, ctx: &mut Interp) -> Result<Value, Value> {
        match self.0 {
            PromiseRepr::Pending(p) => Ok(p),
            PromiseRepr::Ready(r) => {
                let d = Deferred::new(ctx);
                let p = d.promise();
                match r {
                    Ok(v) => d.resolve(ctx, v),
                    Err(e) => d.reject(ctx, e),
                }
                Ok(p)
            }
        }
    }
}

// =============================================================================================
// Async ops: work off the JS thread
// =============================================================================================
//
// The engine is single-threaded: JS values (and so `Deferred`) never leave the JS thread. An
// async op splits in two:
//   1. on the JS thread it converts its arguments into owned `Send` data, creates a `Deferred`
//      and registers it with the host's event loop, getting back a [`Completer`] (`Send`);
//   2. the work runs elsewhere — a worker-pool thread ([`Interp::spawn_blocking`]), a dedicated
//      thread, or whatever I/O completion the host drives — and finishes by handing the
//      `Completer` a result. The loop wakes, and on the JS thread converts that result with
//      `IntoRet` and settles the promise (reactions run in the loop's microtask checkpoint).
//
// The loop itself is the host's business ([`AsyncHost`], installed by lumen-runtime). Without
// one, `spawn_blocking` runs the work inline and settles immediately (still a promise, still
// asynchronous to JS), and completers queue their results until [`Interp::poll_async`].

/// A finished async result on its way back to the JS thread: turns it into the settled value
/// (`Err` rejects). Built by [`Completer::complete`]; run by the host on the JS thread.
pub type Settle = Box<dyn FnOnce(&mut Interp) -> Result<Value, Value> + Send>;

/// The event loop's side of async ops. lumen-runtime installs one per realm
/// ([`Interp::set_async_host`]); an embedder with its own loop implements it the same way.
pub trait AsyncHost: 'static {
    /// Keep the loop alive for `deferred` until the returned completer delivers a [`Settle`];
    /// the host then runs it on the JS thread and settles `deferred` with the outcome.
    fn pending(&self, ctx: &mut Interp, deferred: Deferred) -> Completer;
    /// Run `job` off the JS thread (a worker pool, or its own thread when `dedicated`: work that
    /// may block for an unbounded time must not occupy a pool slot).
    fn spawn(&self, job: Box<dyn FnOnce() + Send>, dedicated: bool);
}

struct AsyncHostSlot(Rc<dyn AsyncHost>);

/// Settles one pending promise from any thread. Dropping it unsettled rejects the promise
/// (an `Error: async operation was dropped`) rather than leaving the loop waiting forever.
pub struct Completer {
    send: Option<Box<dyn FnOnce(Settle) + Send>>,
}

impl Completer {
    /// A completer delivering its [`Settle`] through `send` (for [`AsyncHost`] implementations).
    pub fn new(send: impl FnOnce(Settle) + Send + 'static) -> Completer {
        Completer {
            send: Some(Box::new(send)),
        }
    }
    /// Settle with `r`, converted on the JS thread: `Ok`/plain values fulfil, `Err` rejects
    /// (`Result<T, E>` with `E: Into<OpError>`, e.g. `io::Error`, `String`, `OpError`).
    pub fn complete<R: IntoRet<JsHost> + Send + 'static>(mut self, r: R) {
        if let Some(send) = self.send.take() {
            send(Box::new(move |ctx: &mut Interp| r.into_ret(ctx)));
        }
    }
    /// Settle with a hand-written conversion (runs on the JS thread).
    pub fn settle_with(
        mut self,
        f: impl FnOnce(&mut Interp) -> Result<Value, Value> + Send + 'static,
    ) {
        if let Some(send) = self.send.take() {
            send(Box::new(f));
        }
    }
}

impl Drop for Completer {
    fn drop(&mut self) {
        if let Some(send) = self.send.take() {
            send(Box::new(|ctx: &mut Interp| {
                Err(ctx.make_error("Error", "async operation was dropped without completing"))
            }));
        }
    }
}

/// Completions queued for [`Interp::poll_async`] when no [`AsyncHost`] is installed.
#[derive(Default)]
struct AsyncInbox {
    next: u64,
    pending: FastMap<u64, Deferred>,
    ready: std::sync::Arc<std::sync::Mutex<Vec<(u64, Settle)>>>,
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// An `OpError` that crosses threads: the class/message/code of an error built off the JS thread.
/// (`OpError` itself may carry a thrown JS value, which is not `Send`.)
#[derive(Clone, Debug)]
pub struct SendError {
    pub class: &'static str,
    pub message: String,
    pub code: Option<Cow<'static, str>>,
}

impl SendError {
    pub fn new(class: &'static str, message: impl Into<String>) -> SendError {
        SendError {
            class,
            message: message.into(),
            code: None,
        }
    }
    pub fn with_code(mut self, c: impl Into<Cow<'static, str>>) -> SendError {
        self.code = Some(c.into());
        self
    }
}

impl From<SendError> for OpError {
    fn from(e: SendError) -> OpError {
        let err = OpError::new(e.class, e.message);
        match e.code {
            Some(c) => err.with_code(c),
            None => err,
        }
    }
}

/// The JS error class of a language-neutral [`NativeError`] kind (the Python mapping lives in
/// `lumen_py::bind`; the table is in `lumen_common::native`).
fn native_error_class(kind: lumen_common::native::ErrorKind) -> &'static str {
    use lumen_common::native::ErrorKind as K;
    match kind {
        K::Type | K::Buffer => "TypeError",
        K::Value | K::Overflow | K::Index | K::ZeroDivision | K::Memory => "RangeError",
        K::Key | K::Runtime | K::Os(_) | K::NotImplemented => "Error",
    }
}

impl From<NativeError> for SendError {
    fn from(e: NativeError) -> SendError {
        SendError {
            class: native_error_class(e.kind),
            message: e.message.into_owned(),
            code: e.code,
        }
    }
}

impl From<NativeError> for OpError {
    fn from(e: NativeError) -> OpError {
        SendError::from(e).into()
    }
}

impl Interp {
    /// Wake an owner event loop when a fallback completer delivers a settlement.
    pub fn set_async_waker(&mut self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) {
        if !self.host_state.has::<AsyncInbox>() {
            self.host_state.put(AsyncInbox::default());
        }
        self.host_state.get_mut::<AsyncInbox>().unwrap().wake = Some(wake);
    }

    /// Install the realm's event loop hooks (see [`AsyncHost`]).
    pub fn set_async_host(&mut self, host: impl AsyncHost) {
        self.host_state.put(AsyncHostSlot(Rc::new(host)));
    }

    fn async_host(&self) -> Option<Rc<dyn AsyncHost>> {
        self.host_state.get::<AsyncHostSlot>().map(|s| s.0.clone())
    }

    /// Register `deferred` with the event loop and get a `Send` handle that settles it from any
    /// thread — the primitive under every async op (I/O completions, readiness callbacks,
    /// worker threads). Without an [`AsyncHost`], settlements wait for [`Interp::poll_async`].
    pub fn completer(&mut self, deferred: Deferred) -> Completer {
        if let Some(host) = self.async_host() {
            return host.pending(self, deferred);
        }
        if !self.host_state.has::<AsyncInbox>() {
            self.host_state.put(AsyncInbox::default());
        }
        let inbox = self
            .host_state
            .get_mut::<AsyncInbox>()
            .expect("just installed");
        let id = inbox.next;
        inbox.next += 1;
        inbox.pending.insert(id, deferred);
        let ready = std::sync::Arc::clone(&inbox.ready);
        let wake = inbox.wake.clone();
        Completer::new(move |settle| {
            ready
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id, settle));
            if let Some(wake) = wake {
                wake();
            }
        })
    }

    /// Settle whatever completers delivered so far when no [`AsyncHost`] is installed (a host
    /// loop does this itself). Returns how many promises settled; run microtasks afterwards.
    pub fn poll_async(&mut self) -> usize {
        let Some(inbox) = self.host_state.get_mut::<AsyncInbox>() else {
            return 0;
        };
        let ready = std::mem::take(
            &mut *inbox
                .ready
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut n = 0;
        for (id, settle) in ready {
            let Some(d) = self
                .host_state
                .get_mut::<AsyncInbox>()
                .and_then(|inbox| inbox.pending.remove(&id))
            else {
                continue;
            };
            match settle(self) {
                Ok(v) => self.resolve_promise(&d.promise, v),
                Err(e) => self.reject_promise(&d.promise, e),
            }
            n += 1;
        }
        n
    }

    /// Whether completers created without an [`AsyncHost`] are still outstanding.
    pub fn has_pending_async(&self) -> bool {
        self.host_state
            .get::<AsyncInbox>()
            .is_some_and(|inbox| !inbox.pending.is_empty())
    }

    /// Run `work` on the host's worker pool and return a promise of its result (converted with
    /// `IntoRet` back on the JS thread; an `Err` rejects). This is what `#[op(async)]` expands to.
    /// Without an [`AsyncHost`] the work runs inline and the promise is already settled.
    pub fn spawn_blocking<R: IntoRet<JsHost> + Send + 'static>(
        &mut self,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Promise<R> {
        self.spawn_async(work, false)
    }

    /// [`Interp::spawn_blocking`] on a dedicated thread, for work that may block for an
    /// unbounded time (waiting on a socket, a child process) and must not hold a pool slot.
    pub fn spawn_thread<R: IntoRet<JsHost> + Send + 'static>(
        &mut self,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Promise<R> {
        self.spawn_async(work, true)
    }

    fn spawn_async<R: IntoRet<JsHost> + Send + 'static>(
        &mut self,
        work: impl FnOnce() -> R + Send + 'static,
        dedicated: bool,
    ) -> Promise<R> {
        let d = Deferred::new(self);
        let p = Promise::pending(&d);
        match self.async_host() {
            Some(host) => {
                let done = host.pending(self, d);
                host.spawn(Box::new(move || done.complete(work())), dedicated);
            }
            None => d.resolve(self, work()),
        }
        p
    }
}

// =============================================================================================
// Classes
// =============================================================================================

/// Per-interpreter class table: TypeId -> (constructor, prototype).
#[derive(Default)]
struct ClassRegistry {
    map: HashMap<TypeId, (Value, Gc)>,
}

/// Instance data side table: object address -> Rust value. The entry holds a *weak* handle, so
/// it neither keeps the object alive nor lets its address be reused while the entry exists;
/// entries of dead objects are swept (dropping the Rust value) as the table grows, or by
/// [`Interp::sweep_host_objects`].
#[derive(Default)]
struct HostObjects {
    map: FastMap<usize, HostEntry>,
    sweep_at: usize,
}

struct HostEntry {
    _weak: WeakGc,
    data: Rc<dyn Any>,
    retained: Option<Value>,
    identity: bool,
    callbacks_retained: bool,
    view_ref: fn(&Rc<dyn Any>, TypeId) -> Result<(*const dyn Any, Box<dyn Any>), ()>,
    view_mut: fn(&Rc<dyn Any>, TypeId) -> Result<(*mut dyn Any, Box<dyn Any>), ()>,
}

fn host_entry<'a>(i: &'a Interp, v: &Value) -> Option<&'a HostEntry> {
    let o = v.as_obj()?;
    if let Some((target, _)) = i.proxies.get(&(Gc::as_ptr(o) as usize)) { return host_entry(i, target); }
    i.host_state.get::<HostObjects>()?.map.get(&(Gc::as_ptr(o) as usize))
}

fn view_ref<T: Class>(data: &Rc<dyn Any>, ty: TypeId) -> Result<(*const dyn Any, Box<dyn Any>), ()> {
    let rc = data.clone().downcast::<RefCell<T>>().map_err(|_| ())?;
    let borrow = rc.try_borrow().map_err(|_| ())?;
    let ptr = borrow.view(ty).ok_or(())? as *const dyn Any;
    // The guard owns rc and releases the borrow before that allocation.
    let borrow = unsafe { std::mem::transmute::<std::cell::Ref<'_, T>, std::cell::Ref<'static, T>>(borrow) };
    Ok((ptr, Box::new(RefGuard { g: Some(borrow), _rc: rc })))
}

fn view_mut<T: Class>(data: &Rc<dyn Any>, ty: TypeId) -> Result<(*mut dyn Any, Box<dyn Any>), ()> {
    let rc = data.clone().downcast::<RefCell<T>>().map_err(|_| ())?;
    let mut borrow = rc.try_borrow_mut().map_err(|_| ())?;
    let ptr = borrow.view_mut(ty).ok_or(())? as *mut dyn Any;
    let borrow = unsafe { std::mem::transmute::<std::cell::RefMut<'_, T>, std::cell::RefMut<'static, T>>(borrow) };
    Ok((ptr, Box::new(MutGuard { g: Some(borrow), _rc: rc })))
}

/// A bound fn registered with an interpreter (see [`Interp::op_info_of`]).
#[derive(Clone, Copy, Debug)]
pub struct OpInfo {
    pub desc: &'static FnDesc,
    /// Bit `i`: JS argument `i` is a sync non-escaping callback (`SyncFn`).
    pub callbacks: u32,
}

/// Registered ops by native entry address (the JIT's fast-op lookup, see
/// [`Interp::op_info_of`]).
#[derive(Default)]
struct OpRegistry {
    by_fn: FastMap<usize, OpInfo>,
}

fn host_data(i: &Interp, v: &Value) -> Option<Rc<dyn Any>> {
    host_entry(i, v).map(|e| e.data.clone())
}

fn host_objects(i: &mut Interp) -> &mut HostObjects {
    if !i.host_state.has::<HostObjects>() {
        i.host_state.put(HostObjects::default());
    }
    i.host_state.get_mut::<HostObjects>().unwrap()
}

fn new_instance<T: Class>(i: &mut Interp, value: T, proto: Gc) -> Value {
    let obj = Object::new(Some(proto));
    attach_instance(i, &obj, value);
    let target = Value::Obj(obj);
    let indexed = i.host_state.get::<IndexedClasses>().and_then(|r| r.0.get(&TypeId::of::<T>())).cloned();
    if let Some(handler) = indexed {
        crate::builtins::proxy::make_proxy(i, target, handler).unwrap_or_else(|_| panic!("native indexed wrapper"))
    } else { target }
}

#[derive(Default)]
struct IndexedClasses(FastMap<TypeId, Value>);

fn indexed_handler(i: &mut Interp, item: NativeFn, length: NativeFn) -> Value {
    let handler = i.new_object();
    let item = Value::Obj(i.make_native("getitem", 1, item));
    let length = Value::Obj(i.make_native("length", 0, length));
    for (name, entry) in [("get", indexed_get as NativeFn), ("has", indexed_has), ("ownKeys", indexed_keys), ("getOwnPropertyDescriptor", indexed_descriptor), ("set", indexed_set), ("defineProperty", indexed_define), ("deleteProperty", indexed_delete)] {
        let function = crate::builtins::make_bound_len(i, entry, vec![item.clone(), length.clone()], 0.0);
        handler.borrow_mut().props.insert(name, Property::builtin(function));
    }
    Value::Obj(handler)
}

struct IdentityCache<T: 'static, K: 'static> {
    map: std::collections::HashMap<K, WeakValue>,
    sweep_at: usize,
    _class: std::marker::PhantomData<T>,
}

/// Promote identity wrappers with expandos before the cycle collector runs.
pub(crate) fn retain_host_identities(i: &mut Interp) {
    let Some(objects) = i.host_state.get_mut::<HostObjects>() else { return; };
    for entry in objects.map.values_mut() {
        if entry.identity || entry.callbacks_retained {
            let value = entry._weak.upgrade().map(Value::Obj);
            let expando = value.as_ref().is_some_and(|value| value.as_obj().is_some_and(|obj| obj.borrow().props.iter().next().is_some()));
            entry.retained = if entry.callbacks_retained || expando { value } else { None };
        }
    }
}

pub(crate) fn retain_identity_receiver(i: &mut Interp, value: &Value) {
    let Some(object) = value.as_obj() else { return; };
    let key = Gc::as_ptr(object) as usize;
    if let Some(entry) = i.host_state.get_mut::<HostObjects>().and_then(|objects| objects.map.get_mut(&key)) {
        if entry.identity { entry.retained = Some(value.clone()); }
    }
}

fn index_key(value: &Value) -> Option<usize> {
    let Value::Str(key) = value else { return None; };
    let key = key.as_str();
    let index = key.parse::<usize>().ok()?;
    (index.to_string() == key).then_some(index)
}

fn indexed_length(i: &mut Interp, args: &[Value]) -> Result<usize, Value> {
    match i.invoke(args[1].clone(), args[2].clone(), &[])? { Value::Num(n) => Ok(n as usize), _ => Err(i.make_error("TypeError", "indexed length must be a number")) }
}

fn reflect(i: &mut Interp, name: &str, args: &[Value]) -> Result<Value, Value> {
    let global = Value::Obj(i.global.clone());
    let object = i.get_member(&global, "Reflect").map_err(abrupt_value)?;
    let function = i.get_member(&object, name).map_err(abrupt_value)?;
    i.invoke(function, object, args)
}

fn indexed_get(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if let Some(index) = args.get(3).and_then(index_key) {
        if index < indexed_length(i, args)? { i.invoke(args[0].clone(), args[2].clone(), &[Value::Num(index as f64)]) } else { Ok(Value::Undefined) }
    } else { reflect(i, "get", &args[2..]) }
}

fn indexed_has(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if let Some(index) = args.get(3).and_then(index_key) { Ok(Value::Bool(index < indexed_length(i, args)?)) } else { reflect(i, "has", &args[2..]) }
}

fn indexed_set(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if args.get(3).and_then(index_key).is_some() { Ok(Value::Bool(false)) } else { reflect(i, "set", &args[2..]) }
}

fn indexed_define(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if args.get(3).and_then(index_key).is_some() { Ok(Value::Bool(false)) } else { reflect(i, "defineProperty", &args[2..]) }
}

fn indexed_delete(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if args.get(3).and_then(index_key).is_some() { Ok(Value::Bool(false)) } else { reflect(i, "deleteProperty", &args[2..]) }
}

fn indexed_keys(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    let length = indexed_length(i, args)?;
    let own = reflect(i, "ownKeys", &args[2..])?;
    let count = i.get_member(&own, "length").map_err(abrupt_value)?;
    let mut keys: Vec<Value> = (0..length).map(|index| Value::str(index.to_string())).collect();
    if let Value::Num(count) = count { for index in 0..count as usize { keys.push(i.get_member(&own, &index.to_string()).map_err(abrupt_value)?); } }
    Ok(JsHost::from_list(i, keys))
}

fn indexed_descriptor(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    if let Some(index) = args.get(3).and_then(index_key) {
        if index >= indexed_length(i, args)? { return Ok(Value::Undefined); }
        let value = i.invoke(args[0].clone(), args[2].clone(), &[Value::Num(index as f64)])?;
        let descriptor = i.new_object();
        for (name, value) in [("value", value), ("writable", Value::Bool(false)), ("enumerable", Value::Bool(true)), ("configurable", Value::Bool(true))] { descriptor.borrow_mut().props.insert(name, Property::builtin(value)); }
        Ok(Value::Obj(descriptor))
    } else { reflect(i, "getOwnPropertyDescriptor", &args[2..]) }
}

fn attach_instance<T: Class>(i: &mut Interp, obj: &Gc, value: T) {
    let t = host_objects(i);
    if t.map.len() >= t.sweep_at.max(256) {
        sweep(i);
    }
    let t = host_objects(i);
    t.map.insert(
        Gc::as_ptr(obj) as usize,
        HostEntry {
            _weak: Gc::downgrade(obj),
            data: Rc::new(RefCell::new(value)),
            retained: None,
            identity: false,
            callbacks_retained: false,
            view_ref: view_ref::<T>,
            view_mut: view_mut::<T>,
        },
    );
}

fn sweep(i: &mut Interp) -> usize {
    let t = host_objects(i);
    let dead: Vec<usize> = t
        .map
        .iter()
        .filter(|(_, e)| e._weak.strong_count() == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut drop_later = Vec::with_capacity(dead.len());
    for k in &dead {
        drop_later.extend(t.map.remove(k));
    }
    t.sweep_at = t.map.len() * 2;
    // Rust destructors run outside the table borrow.
    drop(drop_later);
    dead.len()
}

fn illegal_constructor(i: &mut Interp, _: Value, _: &[Value]) -> Result<Value, Value> {
    Err(i.make_error("TypeError", "Illegal constructor"))
}

fn registered_class<T: Class>(i: &Interp) -> Option<(Value, Gc)> {
    i.host_state
        .get::<ClassRegistry>()
        .and_then(|r| r.map.get(&TypeId::of::<T>()))
        .cloned()
}

#[cold]
fn unregistered<T: Class>(i: &mut Interp) -> Value {
    let msg = format!(
        "class {} is not registered with this engine (define its module or class first)",
        class_name::<T>()
    );
    i.make_error("TypeError", msg)
}

/// This interpreter's constructor + prototype for `T`, created on first use.
fn class_entry<T: Methods<JsHost>>(i: &mut Interp) -> (Value, Gc) {
    if let Some(e) = registered_class::<T>(i) {
        return e;
    }
    let mut members = Vec::new();
    T::members(&mut members);
    members.retain(|m| m.desc.exposed_to("js"));
    if let (Some(item), Some(length)) = (members.iter().find(|m| m.desc.role == Role::Proto("getitem")), members.iter().find(|m| m.desc.role == Role::Proto("len"))) {
        if !i.host_state.has::<IndexedClasses>() { i.host_state.put(IndexedClasses::default()); }
        let handler = indexed_handler(i, item.entry, length.entry);
        i.host_state.get_mut::<IndexedClasses>().unwrap().0.insert(TypeId::of::<T>(), handler);
    }
    let name = class_name::<T>();
    let base = T::base_class(i).unwrap_or_else(|_| panic!("native base class registration"));
    let base_proto = base.as_ref().and_then(|base| base.as_obj())
        .and_then(|base| base.borrow().props.get("prototype").and_then(|p| p.value().as_obj().cloned()));
    let proto = Object::new(Some(base_proto.unwrap_or_else(|| i.object_proto.clone())));
    let ctor_fn = match members.iter().find(|m| m.desc.role == Role::Constructor) {
        Some(m) => {
            register_op(i, m);
            (m.entry, m.desc.min_pos as usize)
        }
        None => (illegal_constructor as NativeFn, 0),
    };
    let ctor = i.make_native(name, ctor_fn.1, ctor_fn.0);
    {
        let mut c = ctor.borrow_mut();
        c.is_constructor = true;
        if let Some(base) = base.and_then(|base| base.as_obj().cloned()) { c.proto = Some(base); }
        c.props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
    }
    proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(ctor.clone()), true, false, true),
    );
    crate::builtins::set_to_string_tag(i, &proto, name);
    // Accessors: pair getters and setters by name.
    let mut accessors: Vec<(Cow<'static, str>, Option<Value>, Option<Value>)> = Vec::new();
    for m in &members {
        let d = m.desc;
        let js = js_name(d);
        let installed = match d.role {
            Role::Constructor | Role::Function => false,
            Role::Proto(p) => matches!(p, "iter" | "next" | "str" | "len"),
            _ => true,
        };
        if !installed {
            continue;
        }
        register_op(i, m);
        match d.role {
            Role::Method | Role::Proto("next" | "str") => {
                i.def_method(&proto, &js, d.min_pos as usize, m.entry)
            }
            Role::Static => i.def_method(&ctor, &js, d.min_pos as usize, m.entry),
            Role::Proto("iter") => {
                let f = i.make_native("[Symbol.iterator]", 0, m.entry);
                if let Some(key) = crate::builtins::well_known_key(i, "iterator") {
                    proto
                        .borrow_mut()
                        .props
                        .insert(key, Property::builtin(Value::Obj(f)));
                }
            }
            Role::Getter | Role::Proto("len") => {
                let f = Value::Obj(i.make_native(&format!("get {js}"), 0, m.entry));
                match accessors.iter_mut().find(|a| a.0 == js) {
                    Some(a) => a.1 = Some(f),
                    None => accessors.push((js, Some(f), None)),
                }
            }
            Role::Setter => {
                let f = Value::Obj(i.make_native(&format!("set {js}"), 1, m.entry));
                match accessors.iter_mut().find(|a| a.0 == js) {
                    Some(a) => a.2 = Some(f),
                    None => accessors.push((js, None, Some(f))),
                }
            }
            _ => {}
        }
    }
    for (name, get, set) in accessors {
        proto
            .borrow_mut()
            .props
            .insert(&*name, Property::accessor_prop(get, set, false, true));
    }
    let entry = (Value::Obj(ctor), proto);
    if !i.host_state.has::<ClassRegistry>() {
        i.host_state.put(ClassRegistry::default());
    }
    i.host_state
        .get_mut::<ClassRegistry>()
        .unwrap()
        .map
        .insert(TypeId::of::<T>(), entry.clone());
    entry
}

fn register_op(i: &mut Interp, f: &FnItem<JsHost>) {
    if !i.host_state.has::<OpRegistry>() {
        i.host_state.put(OpRegistry::default());
    }
    let info = OpInfo {
        desc: f.desc,
        callbacks: f.callbacks,
    };
    i.host_state
        .get_mut::<OpRegistry>()
        .unwrap()
        .by_fn
        .insert(f.entry as usize, info);
}

// =============================================================================================
// Embedder API on Ctx / Engine
// =============================================================================================

/// Binding-layer methods on the native-function context.
impl Interp {
    pub fn weak_value(&self, value: &Value) -> Option<WeakValue> {
        match value {
            Value::Obj(object) => Some(WeakValue(Gc::downgrade(object))),
            _ => None,
        }
    }

    /// A JS function for a bound fn (registered for [`Interp::op_info_of`]).
    pub fn bound_function(&mut self, f: &FnItem<JsHost>) -> Value {
        register_op(self, f);
        Value::Obj(self.make_native(&js_name(f.desc), f.desc.min_pos as usize, f.entry))
    }

    /// A JS function for `#[op]` `N` (`ctx.op_function::<clamp::Op>()`).
    pub fn op_function<N: Native<JsHost>>(&mut self) -> Value {
        self.bound_function(&FnItem::of::<N>())
    }

    /// A namespace object holding everything `#[module]` `M` declares.
    pub fn module_object<M: Module<JsHost>>(&mut self) -> Result<Value, Value> {
        JsHost::module_object::<M>(self)
    }

    /// Install everything `#[module]` `M` declares directly on `target`.
    pub fn install_module<M: Module<JsHost>>(&mut self, target: &Value) -> Result<(), Value> {
        let Value::Obj(o) = target else {
            return Err(self.make_error("TypeError", "install_module: target must be an object"));
        };
        install_items(self, o, ModuleItems::of::<M>())
    }

    /// The registration record of the bound fn behind `callee`, if it is one (`op_function`,
    /// modules, class members). The optimizing tier uses it (plus [`FnDesc::scalar`]) to call
    /// scalar ops without boxing.
    pub fn op_info_of(&self, callee: &Value) -> Option<OpInfo> {
        let o = callee.as_obj()?;
        let f = match &o.borrow().call {
            Callable::Native(f) => *f,
            _ => return None,
        };
        self.host_state
            .get::<OpRegistry>()?
            .by_fn
            .get(&(f as usize))
            .copied()
    }

    /// The unboxed entry of `callee`, if it is a scalar op.
    pub fn fast_op_of(&self, callee: &Value) -> Option<ScalarEntry> {
        self.op_info_of(callee)?.desc.scalar
    }

    /// The constructor of class `T` in this interpreter (created on first use).
    pub fn class_constructor<T: Methods<JsHost>>(&mut self) -> Value {
        class_entry::<T>(self).0
    }

    /// Wrap a Rust value as a new JS instance of its class.
    pub fn new_instance<T: Methods<JsHost>>(&mut self, value: T) -> Value {
        let (_, proto) = class_entry::<T>(self);
        new_instance(self, value, proto)
    }

    /// Give an existing host object a native class and its prototype.
    pub fn attach_instance<T: Methods<JsHost>>(&mut self, object: &Value, value: T) -> OpResult<()> {
        let Some(object) = object.as_obj() else { return Err(OpError::new("TypeError", "native instance requires an object")); };
        let (_, prototype) = class_entry::<T>(self);
        object.borrow_mut().proto = Some(prototype);
        attach_instance(self, object, value);
        Ok(())
    }

    /// The Rust value behind a class instance (shared handle; borrow it with the `RefCell`
    /// API). `None` when `v` is not a `T` instance.
    pub fn instance_data<T: Class>(&self, v: &Value) -> Option<Rc<RefCell<T>>> {
        host_data(self, v)?.downcast::<RefCell<T>>().ok()
    }

    /// Read a native instance or one of its embedded base classes.
    pub fn with_instance<T: Class, R>(&self, value: &Value, read: impl FnOnce(&T) -> R) -> OpResult<R> {
        let entry = host_entry(self, value).ok_or_else(|| OpError::new("TypeError", "value is not a native instance"))?;
        let (pointer, guard) = (entry.view_ref)(&entry.data, TypeId::of::<T>()).map_err(|_| OpError::new("TypeError", "native instance has the wrong class or is already in use"))?;
        let data = unsafe { &*pointer }.downcast_ref::<T>().expect("matching native projection");
        let result = read(data);
        drop(guard);
        Ok(result)
    }

    /// Keep a host wrapper alive while its native owner retains callbacks.
    pub fn retain_instance(&mut self, v: &Value, retained: bool) {
        let Some(obj) = v.as_obj() else { return; };
        let target = self.proxies.get(&(Gc::as_ptr(obj) as usize)).and_then(|(target, _)| target.as_obj()).unwrap_or(obj);
        let key = Gc::as_ptr(target) as usize;
        if let Some(entry) = host_objects(self).map.get_mut(&key) {
            entry.callbacks_retained = retained;
            entry.retained = retained.then(|| v.clone());
        }
    }

    /// One lazy native wrapper per identity key in this realm.
    pub fn cached_instance<T: Methods<JsHost>, K: Eq + std::hash::Hash + Clone + 'static>(&mut self, key: K, make: impl FnOnce() -> T) -> Value {
        if let Some(value) = self.host_state.get::<IdentityCache<T, K>>().and_then(|cache| cache.map.get(&key)).and_then(WeakValue::upgrade) { return value; }
        let value = self.new_instance(make());
        if let Some(object) = value.as_obj() {
            let target = self.proxies.get(&(Gc::as_ptr(object) as usize)).and_then(|(target, _)| target.as_obj()).unwrap_or(object);
            let ptr = Gc::as_ptr(target) as usize;
            if let Some(entry) = host_objects(self).map.get_mut(&ptr) { entry.identity = true; }
            object.borrow().ic_plain.set(false);
            self.inline_ic_safe.set(false);
        }
        if !self.host_state.has::<IdentityCache<T, K>>() {
            self.host_state.put(IdentityCache::<T, K> { map: std::collections::HashMap::new(), sweep_at: 256, _class: std::marker::PhantomData });
        }
        let weak = self.weak_value(&value).expect("native identity wrapper");
        let cache = self.host_state.get_mut::<IdentityCache<T, K>>().unwrap();
        if cache.map.len() >= cache.sweep_at { cache.map.retain(|_, value| value.upgrade().is_some()); cache.sweep_at = cache.map.len().saturating_mul(2).max(256); }
        cache.map.insert(key, weak);
        value
    }

    /// Drop the Rust values of class instances whose JS objects died. Runs automatically as
    /// instances are created; call it to reclaim promptly (e.g. after `gc`). Returns the count.
    pub fn sweep_host_objects(&mut self) -> usize {
        if !self.host_state.has::<HostObjects>() {
            return 0;
        }
        sweep(self)
    }

    /// Run the cycle collector, then drop the Rust values of dead class instances. Returns
    /// the number of instance values dropped.
    pub fn collect_garbage(&mut self) -> usize {
        self.gc_collect();
        self.sweep_host_objects()
    }

    /// A new pending promise + its settle handle.
    pub fn new_deferred(&mut self) -> Deferred {
        Deferred::new(self)
    }

    /// The realm's global object.
    pub fn global_object(&self) -> Value {
        Value::Obj(self.global.clone())
    }

    /// Native-code compilation and execution counters for this realm.
    pub fn jit_stats(&self) -> crate::JitStats {
        self.jit_stats.get()
    }

    /// `JSON.parse(text)` through the realm's `JSON` object.
    pub fn json_parse(&mut self, text: &str) -> Result<Value, Value> {
        let g = self.global_object();
        let json = self.get_member(&g, "JSON").map_err(abrupt_value)?;
        let parse = self.get_member(&json, "parse").map_err(abrupt_value)?;
        self.invoke(parse, json, &[Value::str(text)])
    }

    /// `globalThis[name]` when it is an object, else a new plain object installed there.
    pub fn namespace_object(&mut self, name: &str) -> Value {
        Value::Obj(self.global_namespace(name))
    }

    fn global_namespace(&mut self, name: &str) -> Gc {
        let g = self.global_object();
        if let Ok(Value::Obj(o)) = self.get_member(&g, name) {
            return o;
        }
        let o = self.new_object();
        self.global
            .borrow_mut()
            .props
            .insert(name, Property::builtin(Value::Obj(o.clone())));
        o
    }
}

impl crate::Engine {
    /// Define `globalThis.<name>` as `#[op]` `N` (`engine.define_fn::<clamp::Op>()`).
    pub fn define_fn<N: Native<JsHost>>(&mut self) {
        let f = self.interp.op_function::<N>();
        let name = js_name(N::DESC);
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(&*name, Property::builtin(f));
    }

    /// Define `globalThis.<module name>` holding everything `#[module]` `M` declares (an
    /// existing object of that name is extended).
    pub fn define_module<M: Module<JsHost>>(&mut self) -> Result<(), Value> {
        let ns = self.interp.global_namespace(M::DESC.name_for("js"));
        install_items(&mut self.interp, &ns, ModuleItems::of::<M>())
    }

    /// Define everything `#[module]` `M` declares as globals.
    pub fn define_globals<M: Module<JsHost>>(&mut self) -> Result<(), Value> {
        let g = self.interp.global.clone();
        install_items(&mut self.interp, &g, ModuleItems::of::<M>())
    }

    /// Define `globalThis.<class name>` as class `T`'s constructor.
    pub fn define_class<T: Methods<JsHost>>(&mut self) {
        let ctor = self.interp.class_constructor::<T>();
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(class_name::<T>(), Property::builtin(ctor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[lumen_bind::module(name = "t")]
    mod t {
        use super::*;

        #[op]
        fn sum(bytes: &[u8]) -> f64 {
            bytes.iter().map(|&x| x as f64).sum()
        }

        #[op]
        fn fill(ctx: &mut Interp, dst: &mut [u8], src: &[u8]) -> Result<Value, Value> {
            // Re-entrant JS sees the lent buffer as detached.
            let g = ctx.global_object();
            let probe = ctx.get_member(&g, "probe").map_err(abrupt_value)?;
            let n = ctx.invoke(probe, Value::Undefined, &[])?;
            for (x, y) in dst.iter_mut().zip(src) {
                *x = *y;
            }
            Ok(n)
        }
    }

    fn run(e: &mut crate::Engine, src: &str) -> String {
        match e.eval_value(src).expect("parse") {
            Ok(v) => match v {
                Value::Str(s) => s.to_string(),
                Value::Num(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => "?".into(),
            },
            Err(v) => {
                let m = e.ctx().get_member(&v, "message").ok();
                format!(
                    "threw {}",
                    m.map(|m| match m {
                        Value::Str(s) => s.to_string(),
                        _ => "?".into(),
                    })
                    .unwrap_or_default()
                )
            }
        }
    }

    #[test]
    fn byte_views_borrow_alias_and_lend() {
        let mut e = crate::Engine::new();
        assert!(e.define_globals::<t::Module>().is_ok());
        assert_eq!(run(&mut e, "sum(new Uint8Array([1,2,3]))"), "6");
        assert_eq!(
            run(&mut e, "sum(new Uint8Array([1,2,3,4]).subarray(1,3))"),
            "5"
        );
        assert_eq!(run(&mut e, "sum(new Uint16Array([257]).buffer)"), "2");
        assert_eq!(
            run(
                &mut e,
                "sum(new DataView(new Uint8Array([9,9,9]).buffer, 1))"
            ),
            "18"
        );
        let r = run(&mut e, "sum(5)");
        assert!(
            r.contains("sum: argument 1 (bytes) must be an ArrayBuffer"),
            "{r}"
        );
        let r = run(
            &mut e,
            "var u = new Uint8Array(4); u.buffer.transfer(); sum(u)",
        );
        assert!(r.contains("detached"), "{r}");
        // Aliasing: dst overlaps src.
        let r = run(&mut e, "var q = new Uint8Array(8); fill(q, q.subarray(2))");
        assert!(
            r.contains("argument 2 (src) overlaps argument 1 (dst)"),
            "{r}"
        );
        // Disjoint halves of one buffer are fine; re-entrant JS sees them detached.
        let r = run(
            &mut e,
            "var w = new Uint8Array([0,0,7,8]); var seen; \
             globalThis.probe = () => { seen = w.length; return 1; }; \
             fill(w.subarray(0,2), w.subarray(2)); `${seen} ${w.length} ${w[0]} ${w[1]}`",
        );
        assert_eq!(r, "0 4 7 8");
        let r = run(
            &mut e,
            "Array.from(new Uint8Array([1,2,3]).map(x => x)).join()",
        );
        assert_eq!(r, "1,2,3");
        // Vec<u8> results adopt the vector.
        let v = uint8array_from_vec(e.ctx(), vec![4, 5, 6]);
        e.ctx()
            .global
            .borrow_mut()
            .props
            .insert("made", Property::builtin(v));
        assert_eq!(
            run(&mut e, "made instanceof Uint8Array && made.join()"),
            "4,5,6"
        );
    }

    #[test]
    fn weak_value_preserves_live_identity_without_rooting_dead_objects() {
        let mut engine = crate::Engine::new();
        let value = Value::Obj(engine.ctx().new_object());
        let weak = engine.ctx().weak_value(&value).unwrap();
        assert!(matches!(weak.upgrade(), Some(Value::Obj(_))));
        drop(value);
        engine.ctx().collect_garbage();
        assert!(weak.upgrade().is_none());
    }
}
