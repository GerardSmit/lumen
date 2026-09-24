//! Typed Rust <-> JS conversions and the runtime behind the binding macros (`#[lumen::op]`,
//! `#[lumen::class]`, `#[lumen::methods]`; feature `macros`). Everything here is reachable
//! through [`crate::embed`] and usable without the macros: [`FromJs`] / [`IntoJs`] are ordinary
//! public traits an embedder implements for its own types.
//!
//! # How a generated wrapper works
//! A wrapper is a plain [`NativeFn`]. It builds an [`ArgCx`] over `(ctx, this, args)`, converts
//! each JS argument with `FromJs` (borrowing where the JS storage allows), calls the Rust fn,
//! and converts the result with `IntoJs`. The `ArgCx` owns everything a borrow needs to stay
//! valid (coerced strings, snapshot copies, `RefCell` guards of class instances, lent buffers)
//! and releases it when the wrapper returns.
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
//! back a Rust reference) and rejected for `&mut [u8]`; immutable buffers reject `&mut [u8]`.

use crate::bytebuf::ByteBuf;
use crate::fasthash::FastMap;
use crate::interpreter::{abrupt_value, Interp};
use crate::lstr::LStr;
use crate::value::{Callable, Exotic, Gc, NativeFn, Object, Property, TaInfo, TaKind, Value, WeakGc};
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::rc::Rc;

// =============================================================================================
// Descriptors
// =============================================================================================

/// An unboxed argument / return kind of a fast op entry (see [`FastSig`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FastKind {
    /// `f64` — a JS Number, passed as a double.
    F64,
    /// `i32` — ToInt32 of a JS Number, passed as a 32-bit integer.
    I32,
    /// `u32` — ToUint32 of a JS Number, passed as a 32-bit integer.
    U32,
    /// `bool` — passed and returned as an `i32` that is exactly 0 or 1 (never a C `_Bool`).
    Bool,
    /// Return only: no value (`undefined`).
    Void,
}

/// An untyped code pointer. The real type is `extern "C" fn(<args per FastSig::args>) -> <ret>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FastPtr(pub *const ());
// SAFETY: a code address; carries no data.
unsafe impl Send for FastPtr {}
unsafe impl Sync for FastPtr {}

/// The unboxed entry of a `#[op(fast)]` op: an `extern "C"` function taking the arguments
/// directly (no `Ctx`, no `Value`s, cannot throw). A compiler that has proven the argument
/// kinds calls `entry` instead of the generic [`OpDesc::native`] wrapper.
#[derive(Clone, Copy, Debug)]
pub struct FastSig {
    pub args: &'static [FastKind],
    pub ret: FastKind,
    pub entry: FastPtr,
}

/// A native op: what `#[lumen::op]` generates (as `<fn name>::DESC`) and what
/// [`Engine::define_op`](crate::Engine::define_op) registers. Class members use the same shape.
pub struct OpDesc {
    /// The JS property / function name.
    pub name: &'static str,
    /// The owning class name for class members (`""` for free ops); used in error messages.
    pub owner: &'static str,
    /// The function's `length`: JS parameters before the first optional one.
    pub arity: u32,
    /// JS parameter names (error messages), indexed by JS argument position.
    pub params: &'static [&'static str],
    /// The generic entry: converts, calls, converts back.
    pub native: NativeFn,
    /// The unboxed entry of a `#[op(fast)]` op.
    pub fast: Option<FastSig>,
    /// `OP_*` flags (see [`private`]).
    pub flags: u32,
}

/// Where a value being converted came from — only used to word error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    arg: u32,
    elem: u32,
}

impl Slot {
    /// The receiver (`this`).
    pub const THIS: Slot = Slot {
        arg: u32::MAX,
        elem: u32::MAX,
    };
    /// JS argument `i` (0-based).
    pub const fn arg(i: u32) -> Slot {
        Slot { arg: i, elem: u32::MAX }
    }
    /// Element `i` of the array at this slot.
    pub const fn elem(self, i: u32) -> Slot {
        Slot {
            arg: self.arg,
            elem: i,
        }
    }
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
struct Scratch {
    more_borrows: Vec<ByteBorrow>,
    lent: Vec<(usize, ByteBuf)>,
    strs: Vec<LStr>,
    vals: Vec<Box<[Value]>>,
    copies: Vec<Box<[u8]>>,
    guards: Vec<Box<dyn Any>>,
}

const UNDEF: &Value = &Value::Undefined;

/// The per-call conversion context handed to [`FromJs::from_js`]. It owns whatever a borrowed
/// argument needs to stay valid until the op returns.
pub struct ArgCx<'s> {
    interp: *mut Interp,
    this: &'s Value,
    args: &'s [Value],
    desc: &'static OpDesc,
    flags: u32,
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
        desc: &'static OpDesc,
        flags: u32,
    ) -> ArgCx<'s> {
        ArgCx {
            interp: ctx,
            this,
            args,
            desc,
            flags,
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
        self.flags & private::OP_COERCE != 0
    }

    /// The op's descriptor.
    pub fn desc(&self) -> &'static OpDesc {
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

    /// Convert JS argument `i` (missing arguments convert from `undefined`).
    #[inline]
    pub fn arg<'a, T: FromJs<'a>>(&'a self, i: u32) -> Result<T, Value> {
        let v = self.args.get(i as usize).unwrap_or(UNDEF);
        T::from_js(self, v, Slot::arg(i))
    }

    /// Convert the receiver.
    #[inline]
    pub fn this_arg<'a, T: FromJs<'a>>(&'a self) -> Result<T, Value> {
        T::from_js(self, self.this, Slot::THIS)
    }

    #[inline]
    fn interp(&self) -> &Interp {
        assert!(!self.in_ctx.get(), "ArgCx used while its Ctx is lent out");
        // SAFETY: `interp` came from a `&mut Interp` that outlives `self`; no `&mut` derived
        // from it is live (checked above).
        unsafe { &*self.interp }
    }

    /// # Safety
    /// The returned reference must be dead before any other use of `self`'s interpreter.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    unsafe fn interp_mut(&self) -> &mut Interp {
        assert!(!self.in_ctx.get(), "ArgCx used while its Ctx is lent out");
        &mut *self.interp
    }

    fn scratch_ref(&self) -> Option<&Scratch> {
        // SAFETY: see `scratch`.
        unsafe { self.scratch.get().as_ref() }
    }

    #[allow(clippy::mut_from_ref)]
    fn scratch(&self) -> &mut Scratch {
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
    pub fn with_state<T: 'static, R>(&self, f: impl FnOnce(&mut State<T>) -> R) -> Result<R, Value> {
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
        // SAFETY: `State<T>` is `repr(transparent)` over `T`.
        let st = unsafe { &mut *(slot as *mut T as *mut State<T>) };
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
        if let Some((_, b)) = s.lent.iter_mut().find(|(k, _)| *k == key) {
            return Some(b.as_mut_ptr());
        }
        let ctx = unsafe { self.interp_mut() };
        let mut buf = ctx.array_buffers.remove(&key)?;
        let p = buf.as_mut_ptr();
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
    /// they are back; moving a `ByteBuf` never moves its heap data, so earlier borrows stay valid.
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
    pub fn view_bytes(&self, v: &Value, at: Slot, mutable: bool) -> Result<(*mut u8, usize), Value> {
        let (key, off, len) = match resolve_view(self.interp(), v) {
            Ok(r) => r,
            Err(ViewErr::Detached) if self.scratch_ref().is_some_and(|s| !s.lent.is_empty()) => {
                // Possibly a buffer this call already lent out (a second view of it).
                self.resolve_with_lent(v).map_err(|e| self.view_error(e, at))?
            }
            Err(e) => return Err(self.view_error(e, at)),
        };
        let i = self.interp();
        if let Some(&id) = i.shared_buffers.get(&key) {
            if mutable {
                return Err(self.type_error(at, "must not be a SharedArrayBuffer view (&mut [u8])"));
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
        if mutable && i.immutable_buffers.contains(&key) {
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
                let lend_now = self.flags & private::OP_CTX != 0
                    || self.scratch_ref().is_some_and(|s| !s.lent.is_empty());
                if lend_now {
                    match self.lend(key) {
                        Some(p) => p,
                        None => return Err(self.view_error(ViewErr::Detached, at)),
                    }
                } else {
                    let ctx = unsafe { self.interp_mut() };
                    match ctx.array_buffers.get_mut(&key) {
                        Some(b) => b.as_mut_ptr(),
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
                self.resolve_with_lent(v).map_err(|e| self.view_error(e, at))?
            }
            Err(e) => return Err(self.view_error(e, at)),
        };
        let i = self.interp();
        if let Some(&id) = i.shared_buffers.get(&key) {
            return Ok(crate::interpreter::shared_mem_get(id)
                .map(|m| m.lock().unwrap().get(off..off + len).unwrap_or_default().to_vec())
                .unwrap_or_default());
        }
        if let Some(b) = i.array_buffers.get(&key) {
            return Ok(b[off..off + len].to_vec());
        }
        // Lent by this very call: read the lent copy.
        if let Some(s) = self.scratch_ref() {
            if let Some((_, b)) = s.lent.iter().find(|(k, _)| *k == key) {
                return Ok(b[off..off + len].to_vec());
            }
        }
        Err(self.view_error(ViewErr::Detached, at))
    }

    fn view_error(&self, e: ViewErr, at: Slot) -> Value {
        match e {
            ViewErr::NotView => {
                self.type_error(at, "must be an ArrayBuffer, a TypedArray (Uint8Array, Buffer, ...) or a DataView")
            }
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
        if self.desc.owner.is_empty() {
            self.desc.name.to_string()
        } else {
            format!("{}.{}", self.desc.owner, self.desc.name)
        }
    }

    fn slot_label(&self, s: Slot) -> String {
        let mut out = if s.arg == u32::MAX {
            "receiver (this)".to_string()
        } else {
            match self.desc.params.get(s.arg as usize) {
                Some(n) => format!("argument {} ({})", s.arg + 1, n),
                None => format!("argument {}", s.arg + 1),
            }
        };
        if s.elem != u32::MAX {
            out.push_str(&format!(" element {}", s.elem));
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

    /// Convert the op's result (the tail of every wrapper).
    #[inline]
    pub fn ret<R: IntoJs>(&self, r: R) -> Result<Value, Value> {
        if R::MAY_RUN_JS {
            self.before_js();
        }
        r.into_js(unsafe { self.interp_mut() })
    }

    // ---- classes ----

    /// The `new.target` of a class-constructor call; throws when called without `new`.
    pub fn new_target(&self) -> Result<Value, Value> {
        let i = self.interp();
        if !i.constructing {
            let msg = format!(
                "Class constructor {} cannot be invoked without 'new'",
                self.desc.owner
            );
            return Err(i.make_error("TypeError", msg));
        }
        Ok(i.new_target.clone())
    }

    /// Finish a class constructor: wrap the Rust value in an instance whose prototype comes
    /// from `new_target` (so JS subclasses get their own prototype and still pass brand checks).
    pub fn construct_ret<T: Class, R: CtorReturn<T>>(&self, nt: Value, r: R) -> Result<Value, Value> {
        self.before_js();
        let ctx = unsafe { self.interp_mut() };
        let value = r.into_ctor_result().map_err(|e| e.to_value(ctx))?;
        // `super(...)` from a JS subclass runs a native parent constructor with the derived
        // instance already allocated as `this`: attach the Rust value to it directly (returning
        // it unchanged, so the engine's native-parent graft has nothing to move).
        if let Value::Obj(o) = self.this {
            attach_instance(ctx, o, value);
            return Ok(self.this.clone());
        }
        let (_, class_proto) = class_entry::<T>(ctx);
        let proto = match &nt {
            Value::Obj(_) => match ctx.get_member(&nt, "prototype").map_err(abrupt_value)? {
                Value::Obj(p) => p,
                _ => class_proto,
            },
            _ => class_proto,
        };
        Ok(new_instance(ctx, value, proto))
    }

    fn class_rc<T: Class>(&self, v: &Value, at: Slot) -> Result<Rc<RefCell<T>>, Value> {
        match host_data(self.interp(), v).and_then(|d| d.downcast::<RefCell<T>>().ok()) {
            Some(rc) => Ok(rc),
            None if at == Slot::THIS => {
                let msg = format!(
                    "{}: illegal invocation (receiver is not a {})",
                    self.label(),
                    T::NAME
                );
                Err(self.interp().make_error("TypeError", msg))
            }
            None => Err(self.type_error(at, &format!("must be a {}", T::NAME))),
        }
    }

    /// Borrow a class instance shared (`&self` receivers, `&T` arguments).
    pub fn class_ref<'a, T: Class>(&'a self, v: &Value, at: Slot) -> Result<&'a T, Value> {
        let rc = self.class_rc::<T>(v, at)?;
        let r = match rc.try_borrow() {
            Ok(r) => r,
            Err(_) => return Err(self.borrow_conflict::<T>(at)),
        };
        // SAFETY: the guard keeps `rc` (and so the RefCell) alive, and drops the `Ref` first.
        let r: std::cell::Ref<'static, T> = unsafe { std::mem::transmute(r) };
        let p: *const T = &*r;
        self.scratch().guards.push(Box::new(RefGuard { g: Some(r), _rc: rc }));
        Ok(unsafe { &*p })
    }

    /// Borrow a class instance exclusively (`&mut self` receivers, `&mut T` arguments). A
    /// conflicting borrow (re-entrant call on the same instance) throws a TypeError.
    #[allow(clippy::mut_from_ref)]
    pub fn class_mut<'a, T: Class>(&'a self, v: &Value, at: Slot) -> Result<&'a mut T, Value> {
        let rc = self.class_rc::<T>(v, at)?;
        let r = match rc.try_borrow_mut() {
            Ok(r) => r,
            Err(_) => return Err(self.borrow_conflict::<T>(at)),
        };
        // SAFETY: as in `class_ref`.
        let mut r: std::cell::RefMut<'static, T> = unsafe { std::mem::transmute(r) };
        let p: *mut T = &mut *r;
        self.scratch().guards.push(Box::new(MutGuard { g: Some(r), _rc: rc }));
        Ok(unsafe { &mut *p })
    }

    #[cold]
    fn borrow_conflict<T: Class>(&self, at: Slot) -> Value {
        self.type_error(
            at,
            &format!("is a {} already in use by an enclosing call (re-entrant borrow)", T::NAME),
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
    if o.borrow().props.contains("__abMaxByteLength") {
        return Err(ViewErr::Detached);
    }
    Err(ViewErr::NotView)
}

// =============================================================================================
// FromJs
// =============================================================================================

/// Convert a JS value into a Rust argument. `'a` is the lifetime of the call's [`ArgCx`]:
/// implementations may borrow from the argument value or from storage the context holds.
///
/// Implement it for your own types by delegating to the provided impls (`f64::from_js`, ...)
/// or to `ArgCx` helpers ([`ArgCx::type_error`], [`ArgCx::with_ctx`] for anything that
/// needs the interpreter).
pub trait FromJs<'a>: Sized {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value>;
}

/// Types that may appear as elements of a `Vec<T>` argument or result (a JS Array). Everything
/// except `u8` (a `Vec<u8>` is a byte buffer). Implement it for your own element types.
pub trait ArrayElem {}

impl<'a> FromJs<'a> for Value {
    #[inline]
    fn from_js(_: &'a ArgCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Value> {
        Ok(v.clone())
    }
}

impl<'a> FromJs<'a> for &'a Value {
    #[inline]
    fn from_js(_: &'a ArgCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Value> {
        Ok(v)
    }
}

impl<'a> FromJs<'a> for f64 {
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        match v {
            Value::Num(n) => Ok(*n),
            _ => cx.number_slow(v, at),
        }
    }
}

impl<'a> FromJs<'a> for f32 {
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        f64::from_js(cx, v, at).map(|n| n as f32)
    }
}

macro_rules! int32_from_js {
    ($($t:ty),*) => {$(
        /// JS ToInt32/ToUint32 wrapping (modulo 2^N) of a Number, like WebIDL integer types.
        impl<'a> FromJs<'a> for $t {
            #[inline]
            fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
                let n = f64::from_js(cx, v, at)?;
                Ok(crate::eval::to_int32(n) as $t)
            }
        }
    )*};
}
int32_from_js!(i8, u8, i16, u16, i32, u32);

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
}

macro_rules! int64_from_js {
    ($($t:ty),*) => {$(
        /// A safe-integer Number or a BigInt in range; otherwise RangeError/TypeError.
        impl<'a> FromJs<'a> for $t {
            fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
                cx.int64(v, at, <$t>::MIN as i128, <$t>::MAX as i128).map(|n| n as $t)
            }
        }
    )*};
}
int64_from_js!(i64, u64, isize, usize);

/// A BigInt-typed 64-bit integer: accepts a BigInt (or an integral Number) and always returns
/// a BigInt. Plain `i64`/`u64` return Numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BigI64(pub i64);
/// The unsigned counterpart of [`BigI64`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BigU64(pub u64);

impl<'a> FromJs<'a> for BigI64 {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.int64(v, at, i64::MIN as i128, i64::MAX as i128).map(|n| BigI64(n as i64))
    }
}
impl<'a> FromJs<'a> for BigU64 {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.int64(v, at, 0, u64::MAX as i128).map(|n| BigU64(n as u64))
    }
}

impl<'a> FromJs<'a> for bool {
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        match v {
            Value::Bool(b) => Ok(*b),
            _ if cx.coerce() => Ok(cx.interp().to_boolean(v)),
            _ => Err(cx.type_error(at, "must be a boolean")),
        }
    }
}

impl<'a> FromJs<'a> for &'a str {
    /// Borrows the JS string's UTF-8 storage. Lone surrogates appear as the private-use
    /// scalars U+10F800.. (lumen's internal smuggling scheme), so the `str` is always valid.
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
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
}

impl<'a> FromJs<'a> for String {
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        <&str>::from_js(cx, v, at).map(str::to_owned)
    }
}

impl<'a> FromJs<'a> for Cow<'a, str> {
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        <&str>::from_js(cx, v, at).map(Cow::Borrowed)
    }
}

impl<'a> FromJs<'a> for &'a [u8] {
    /// Zero-copy view of an ArrayBuffer / TypedArray (any element type) / DataView.
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        let (p, n) = cx.view_bytes(v, at, false)?;
        // SAFETY: see the module docs (pointer borrow / lending).
        Ok(unsafe { std::slice::from_raw_parts(p, n) })
    }
}

impl<'a> FromJs<'a> for &'a mut [u8] {
    /// Zero-copy mutable view; writes are visible to JS after the call.
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        let (p, n) = cx.view_bytes(v, at, true)?;
        // SAFETY: as above, plus the per-call alias check.
        Ok(unsafe { std::slice::from_raw_parts_mut(p, n) })
    }
}

impl<'a> FromJs<'a> for Vec<u8> {
    /// A copy of a view's bytes.
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.view_copy(v, at)
    }
}

impl<'a, T: FromJs<'a>> FromJs<'a> for Option<T> {
    /// `undefined` (or a missing argument) and `null` are `None`.
    #[inline]
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        match v {
            Value::Undefined | Value::Null => Ok(None),
            _ => T::from_js(cx, v, at).map(Some),
        }
    }
}

impl<'a, T: FromJs<'a> + ArrayElem> FromJs<'a> for Vec<T> {
    /// A JS Array, element by element (reads go through `[[Get]]`, so getters run).
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        let is_array = matches!(v, Value::Obj(o) if o.borrow().exotic == Exotic::Array);
        if !is_array {
            return Err(cx.type_error(at, "must be an array"));
        }
        cx.before_js();
        let items: Box<[Value]> = {
            let ctx = unsafe { cx.interp_mut() };
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
        let s = cx.scratch();
        s.vals.push(items);
        let p: *const [Value] = &**s.vals.last().unwrap();
        // SAFETY: the boxed slice is owned by scratch until the context drops.
        let items: &'a [Value] = unsafe { &*p };
        items
            .iter()
            .enumerate()
            .map(|(k, e)| T::from_js(cx, e, at.elem(k as u32)))
            .collect()
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
        ctx.invoke(self.0.clone(), this, args).map_err(OpError::thrown)
    }
    pub fn from_value(v: Value) -> Option<JsFunction> {
        v.is_callable().then_some(JsFunction(v))
    }
}

impl<'a> FromJs<'a> for JsObject {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        match v {
            Value::Obj(_) => Ok(JsObject(v.clone())),
            _ => Err(cx.type_error(at, "must be an object")),
        }
    }
}

impl<'a> FromJs<'a> for JsFunction {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        if v.is_callable() {
            Ok(JsFunction(v.clone()))
        } else {
            Err(cx.type_error(at, "must be a function"))
        }
    }
}

/// The receiver as an op parameter: `fn f(this: This<Value>, ...)`. Not a JS argument.
pub struct This<T>(pub T);
impl<T> std::ops::Deref for This<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Typed host state as an op parameter: `fn f(st: &mut State<Counter>)` reads the `Counter`
/// the embedder installed with `ctx.op_state().put(Counter { .. })`. Not a JS argument.
#[repr(transparent)]
pub struct State<T>(T);
impl<T> std::ops::Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<T> std::ops::DerefMut for State<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

// =============================================================================================
// IntoJs
// =============================================================================================

/// Convert a Rust result into a JS value. `Err` is a thrown value.
pub trait IntoJs {
    /// Whether `into_js` may run JS (getters, thenables, user code). When it may, byte buffers
    /// the op borrowed are lent first (see the module docs). Keep the default `true` unless the
    /// conversion only allocates.
    const MAY_RUN_JS: bool = true;
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value>;
}

impl IntoJs for () {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::Undefined)
    }
}
impl IntoJs for Value {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(self)
    }
}
impl IntoJs for &Value {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(self.clone())
    }
}
impl IntoJs for bool {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::Bool(self))
    }
}
macro_rules! num_into_js {
    ($($t:ty),*) => {$(
        impl IntoJs for $t {
            const MAY_RUN_JS: bool = false;
            #[inline]
            fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
                Ok(Value::Num(self as f64))
            }
        }
    )*};
}
num_into_js!(f64, f32, i8, u8, i16, u16, i32, u32);

macro_rules! int64_into_js {
    ($($t:ty),*) => {$(
        /// A Number; a value beyond ±(2^53-1) throws a RangeError (return `BigI64`/`BigU64`
        /// for a BigInt instead).
        impl IntoJs for $t {
            const MAY_RUN_JS: bool = false;
            #[inline]
            fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
                let n = self as i128;
                if n.abs() <= 9007199254740991 {
                    Ok(Value::Num(n as f64))
                } else {
                    Err(ctx.make_error(
                        "RangeError",
                        format!("result {self} exceeds Number.MAX_SAFE_INTEGER (return BigI64/BigU64)"),
                    ))
                }
            }
        }
    )*};
}
int64_into_js!(i64, u64, isize, usize);

impl IntoJs for BigI64 {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::bigint_from_i64(self.0))
    }
}
impl IntoJs for BigU64 {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::bigint_from_u64(self.0))
    }
}
impl IntoJs for String {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::from_string(self))
    }
}
impl IntoJs for &str {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::str(self))
    }
}
impl IntoJs for &String {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::str(self.as_str()))
    }
}
impl IntoJs for Cow<'_, str> {
    const MAY_RUN_JS: bool = false;
    #[inline]
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(match self {
            Cow::Borrowed(s) => Value::str(s),
            Cow::Owned(s) => Value::from_string(s),
        })
    }
}
impl IntoJs for char {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(Value::Str(LStr::from(self)))
    }
}
impl IntoJs for Vec<u8> {
    const MAY_RUN_JS: bool = false;
    /// A `Uint8Array` that adopts the vector as its backing store (no copy).
    #[inline]
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        Ok(uint8array_from_vec(ctx, self))
    }
}
impl IntoJs for Box<[u8]> {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        Ok(uint8array_from_vec(ctx, self.into_vec()))
    }
}
impl IntoJs for &[u8] {
    const MAY_RUN_JS: bool = false;
    /// A `Uint8Array` holding a copy.
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        Ok(uint8array_from_vec(ctx, self.to_vec()))
    }
}
impl IntoJs for &mut [u8] {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        Ok(uint8array_from_vec(ctx, self.to_vec()))
    }
}

/// Return an `ArrayBuffer` (instead of a `Uint8Array`) adopting the bytes.
pub struct JsArrayBuffer(pub Vec<u8>);
impl IntoJs for JsArrayBuffer {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        Ok(ctx.make_array_buffer_from(self.0))
    }
}

impl<T: IntoJs> IntoJs for Option<T> {
    const MAY_RUN_JS: bool = T::MAY_RUN_JS;
    /// `None` is `null`.
    #[inline]
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        match self {
            Some(v) => v.into_js(ctx),
            None => Ok(Value::Null),
        }
    }
}

impl<T: IntoJs, E: Into<OpError>> IntoJs for Result<T, E> {
    const MAY_RUN_JS: bool = T::MAY_RUN_JS;
    /// `Err` throws.
    #[inline]
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        match self {
            Ok(v) => v.into_js(ctx),
            Err(e) => Err(e.into().to_value(ctx)),
        }
    }
}

impl<T: IntoJs + ArrayElem> IntoJs for Vec<T> {
    const MAY_RUN_JS: bool = T::MAY_RUN_JS;
    /// A JS Array.
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
        let mut out = Vec::with_capacity(self.len());
        for v in self {
            out.push(v.into_js(ctx)?);
        }
        Ok(ctx.make_array(out))
    }
}

macro_rules! tuple_into_js {
    ($($n:ident),+) => {
        impl<$($n: IntoJs),+> IntoJs for ($($n,)+) {
            const MAY_RUN_JS: bool = false $(|| $n::MAY_RUN_JS)+;
            /// A JS Array.
            #[allow(non_snake_case)]
            fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
                let ($($n,)+) = self;
                let out = vec![$($n.into_js(ctx)?),+];
                Ok(ctx.make_array(out))
            }
        }
    };
}
tuple_into_js!(A);
tuple_into_js!(A, B);
tuple_into_js!(A, B, C);
tuple_into_js!(A, B, C, D);

impl IntoJs for JsObject {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(self.0)
    }
}
impl IntoJs for JsFunction {
    const MAY_RUN_JS: bool = false;
    fn into_js(self, _: &mut Interp) -> Result<Value, Value> {
        Ok(self.0)
    }
}

macro_rules! array_elem {
    ($($t:ty),*) => {$( impl ArrayElem for $t {} )*};
}
array_elem!(
    f64, f32, i8, i16, u16, i32, u32, i64, u64, isize, usize, bool, char, String, Value, JsObject,
    JsFunction, BigI64, BigU64, Vec<u8>
);
impl ArrayElem for &str {}
impl ArrayElem for Cow<'_, str> {}
impl<T: ArrayElem> ArrayElem for Option<T> {}
impl<T: ArrayElem> ArrayElem for Vec<T> {}

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
                let e = ctx.make_error(class, message.into_owned());
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
        use std::io::ErrorKind as K;
        let code = match e.kind() {
            K::NotFound => Some("ENOENT"),
            K::PermissionDenied => Some("EACCES"),
            K::AlreadyExists => Some("EEXIST"),
            K::ConnectionRefused => Some("ECONNREFUSED"),
            K::ConnectionReset => Some("ECONNRESET"),
            K::ConnectionAborted => Some("ECONNABORTED"),
            K::TimedOut => Some("ETIMEDOUT"),
            K::BrokenPipe => Some("EPIPE"),
            K::InvalidInput => Some("EINVAL"),
            K::AddrInUse => Some("EADDRINUSE"),
            K::WouldBlock => Some("EAGAIN"),
            K::Interrupted => Some("EINTR"),
            K::Unsupported => Some("ENOTSUP"),
            K::DirectoryNotEmpty => Some("ENOTEMPTY"),
            K::IsADirectory => Some("EISDIR"),
            K::NotADirectory => Some("ENOTDIR"),
            _ => None,
        };
        let err = OpError::error(e.to_string());
        match code {
            Some(c) => err.with_code(c),
            None => err,
        }
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
    pub fn resolve<T: IntoJs>(self, ctx: &mut Interp, v: T) {
        match v.into_js(ctx) {
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

impl<T: IntoJs> IntoJs for Promise<T> {
    fn into_js(self, ctx: &mut Interp) -> Result<Value, Value> {
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
// Classes
// =============================================================================================

/// A Rust type exposed as a JS class (implemented by `#[lumen::class]`). The Rust value lives
/// in an `Rc<RefCell<T>>` attached to the JS instance; methods borrow it per call.
pub trait Class: Sized + 'static {
    /// The JS class name.
    const NAME: &'static str;
    /// Constructor + members (implemented by `#[lumen::methods]`).
    fn class_desc() -> &'static ClassDesc;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    Getter,
    Setter,
    StaticMethod,
    StaticGetter,
    StaticSetter,
}

pub struct MemberDesc {
    pub kind: MemberKind,
    /// The wrapper; `op.name` is the JS property name.
    pub op: &'static OpDesc,
}

pub struct ClassDesc {
    pub name: &'static str,
    /// `None`: `new X()` throws "Illegal constructor" (instances come from Rust only).
    pub constructor: Option<&'static OpDesc>,
    pub members: &'static [MemberDesc],
}

/// What a `#[constructor]` may return: `T` or `Result<T, E>`.
pub trait CtorReturn<T> {
    fn into_ctor_result(self) -> Result<T, OpError>;
}
impl<T: Class> CtorReturn<T> for T {
    fn into_ctor_result(self) -> Result<T, OpError> {
        Ok(self)
    }
}
impl<T: Class, E: Into<OpError>> CtorReturn<T> for Result<T, E> {
    fn into_ctor_result(self) -> Result<T, OpError> {
        self.map_err(Into::into)
    }
}

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
}

/// Registered ops by native entry address (the JIT's fast-op lookup, see
/// [`Interp::op_desc_of`]).
#[derive(Default)]
struct OpRegistry {
    by_fn: FastMap<usize, &'static OpDesc>,
}

fn host_data(i: &Interp, v: &Value) -> Option<Rc<dyn Any>> {
    let o = v.as_obj()?;
    let t = i.host_state.get::<HostObjects>()?;
    t.map.get(&(Gc::as_ptr(o) as usize)).map(|e| e.data.clone())
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
    Value::Obj(obj)
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

/// This interpreter's constructor + prototype for `T`, created on first use.
fn class_entry<T: Class>(i: &mut Interp) -> (Value, Gc) {
    if let Some(e) = i
        .host_state
        .get::<ClassRegistry>()
        .and_then(|r| r.map.get(&TypeId::of::<T>()))
    {
        return e.clone();
    }
    let desc = T::class_desc();
    let proto = Object::new(Some(i.object_proto.clone()));
    let (ctor_fn, arity) = match desc.constructor {
        Some(d) => (d.native, d.arity as usize),
        None => (illegal_constructor as NativeFn, 0),
    };
    let ctor = i.make_native(desc.name, arity, ctor_fn);
    {
        let mut c = ctor.borrow_mut();
        c.is_constructor = true;
        c.props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
    }
    proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(ctor.clone()), true, false, true),
    );
    crate::builtins::set_to_string_tag(i, &proto, desc.name);
    // Accessors: pair getters and setters by (static, name).
    let mut accessors: Vec<(bool, &'static str, Option<Value>, Option<Value>)> = Vec::new();
    for m in desc.members {
        let op = m.op;
        register_op(i, op);
        let is_static = matches!(
            m.kind,
            MemberKind::StaticMethod | MemberKind::StaticGetter | MemberKind::StaticSetter
        );
        let target = if is_static { &ctor } else { &proto };
        match m.kind {
            MemberKind::Method | MemberKind::StaticMethod => {
                i.def_method(target, op.name, op.arity as usize, op.native);
            }
            MemberKind::Getter | MemberKind::StaticGetter => {
                let f = i.make_native(&format!("get {}", op.name), 0, op.native);
                match accessors.iter_mut().find(|a| a.0 == is_static && a.1 == op.name) {
                    Some(a) => a.2 = Some(Value::Obj(f)),
                    None => accessors.push((is_static, op.name, Some(Value::Obj(f)), None)),
                }
            }
            MemberKind::Setter | MemberKind::StaticSetter => {
                let f = i.make_native(&format!("set {}", op.name), 1, op.native);
                match accessors.iter_mut().find(|a| a.0 == is_static && a.1 == op.name) {
                    Some(a) => a.3 = Some(Value::Obj(f)),
                    None => accessors.push((is_static, op.name, None, Some(Value::Obj(f)))),
                }
            }
        }
    }
    for (is_static, name, get, set) in accessors {
        let target = if is_static { &ctor } else { &proto };
        target
            .borrow_mut()
            .props
            .insert(name, Property::accessor_prop(get, set, false, true));
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

fn register_op(i: &mut Interp, op: &'static OpDesc) {
    if !i.host_state.has::<OpRegistry>() {
        i.host_state.put(OpRegistry::default());
    }
    i.host_state
        .get_mut::<OpRegistry>()
        .unwrap()
        .by_fn
        .insert(op.native as usize, op);
}

// =============================================================================================
// Embedder API on Ctx / Engine
// =============================================================================================

/// Binding-layer methods on the native-function context.
impl Interp {
    /// A JS function for `op` (registered for [`Interp::op_desc_of`]).
    pub fn op_function(&mut self, op: &'static OpDesc) -> Value {
        register_op(self, op);
        Value::Obj(self.make_native(op.name, op.arity as usize, op.native))
    }

    /// The descriptor of the op behind `callee`, if it is a function created from an
    /// [`OpDesc`] (`define_op`, `define_ops`, `op_function`, class members). The optimizing tier
    /// uses this (plus [`OpDesc::fast`]) to call `#[op(fast)]` ops without boxing.
    pub fn op_desc_of(&self, callee: &Value) -> Option<&'static OpDesc> {
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

    /// The unboxed entry of `callee`, if it is a `#[op(fast)]` op.
    pub fn fast_op_of(&self, callee: &Value) -> Option<FastSig> {
        self.op_desc_of(callee)?.fast
    }

    /// The constructor of class `T` in this interpreter (created on first use).
    pub fn class_constructor<T: Class>(&mut self) -> Value {
        class_entry::<T>(self).0
    }

    /// Wrap a Rust value as a new JS instance of its class.
    pub fn new_instance<T: Class>(&mut self, value: T) -> Value {
        let (_, proto) = class_entry::<T>(self);
        new_instance(self, value, proto)
    }

    /// The Rust value behind a class instance (shared handle; borrow it with the `RefCell`
    /// API). `None` when `v` is not a `T` instance.
    pub fn instance_data<T: Class>(&self, v: &Value) -> Option<Rc<RefCell<T>>> {
        host_data(self, v)?.downcast::<RefCell<T>>().ok()
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

    /// `JSON.parse(text)` through the realm's `JSON` object.
    pub fn json_parse(&mut self, text: &str) -> Result<Value, Value> {
        let g = self.global_object();
        let json = self.get_member(&g, "JSON").map_err(abrupt_value)?;
        let parse = self.get_member(&json, "parse").map_err(abrupt_value)?;
        self.invoke(parse, json, &[Value::str(text)])
    }
}

impl crate::Engine {
    /// Define `globalThis.<op.name>` from an op descriptor (`engine.define_op(&clamp::DESC)`).
    pub fn define_op(&mut self, op: &'static OpDesc) {
        let f = self.interp.op_function(op);
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(op.name, Property::builtin(f));
    }

    /// Define ops on `globalThis.<namespace>` (created as a plain object if absent, extended if
    /// it already is one): `engine.define_ops("x", &[&aes_ecb::DESC, &clamp::DESC])` or
    /// `engine.define_ops("x", lumen::ops![aes_ecb, clamp])`.
    pub fn define_ops(&mut self, namespace: &str, ops: &[&'static OpDesc]) {
        let g = self.interp.global_object();
        let existing = match self.interp.get_member(&g, namespace) {
            Ok(Value::Obj(o)) => Some(o),
            _ => None,
        };
        let ns = match existing {
            Some(o) => o,
            None => {
                let o = self.interp.new_object();
                self.interp
                    .global
                    .borrow_mut()
                    .props
                    .insert(namespace, Property::builtin(Value::Obj(o.clone())));
                o
            }
        };
        for op in ops {
            let f = self.interp.op_function(op);
            ns.borrow_mut().props.insert(op.name, Property::builtin(f));
        }
    }

    /// Define `globalThis.<T::NAME>` as class `T`'s constructor.
    pub fn define_class<T: Class>(&mut self) {
        let ctor = self.interp.class_constructor::<T>();
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(T::NAME, Property::builtin(ctor));
    }
}

/// Support items for macro-generated code. Not a stable API.
#[doc(hidden)]
pub mod private {
    use super::*;

    pub const OP_COERCE: u32 = 1;
    pub const OP_CTX: u32 = 2;
    pub const OP_ASYNC: u32 = 4;

    /// Implemented by `#[methods]`; `#[class]`'s `Class::class_desc` forwards here (a class
    /// without members still needs an empty `#[methods] impl T {}`).
    pub trait ClassMethods {
        fn desc() -> &'static ClassDesc;
    }

    /// `#[op(async)]`: turn the synchronous outcome (including argument errors) into a
    /// settled promise.
    pub fn async_ret(ctx: &mut Interp, r: Result<Value, Value>) -> Result<Value, Value> {
        let d = Deferred::new(ctx);
        let p = d.promise();
        match r {
            Ok(v) => ctx.resolve_promise(&p, v),
            Err(e) => ctx.reject_promise(&p, e),
        }
        Ok(p)
    }

    /// `IntoJs` for a `#[class]` type.
    pub fn class_into_js<T: Class>(ctx: &mut Interp, v: T) -> Result<Value, Value> {
        Ok(ctx.new_instance(v))
    }

    /// `FromJs` for `&T` / `&mut T` of a `#[class]` type.
    pub fn class_ref<'a, T: Class>(cx: &'a ArgCx<'_>, v: &Value, at: Slot) -> Result<&'a T, Value> {
        cx.class_ref(v, at)
    }
    #[allow(clippy::mut_from_ref)]
    pub fn class_mut<'a, T: Class>(
        cx: &'a ArgCx<'_>,
        v: &Value,
        at: Slot,
    ) -> Result<&'a mut T, Value> {
        cx.class_mut(v, at)
    }

    pub use super::ArgCx;
}

impl<'a, T: Class> FromJs<'a> for Rc<RefCell<T>> {
    /// A shared handle to a class instance's Rust value (no borrow held during the call).
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        cx.class_rc::<T>(v, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum_native(ctx: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
        let cx = ArgCx::new(ctx, &this, args, &SUM, 0);
        let b: &[u8] = cx.arg(0)?;
        let r: f64 = b.iter().map(|&x| x as f64).sum();
        cx.ret(r)
    }
    static SUM: OpDesc = OpDesc {
        name: "sum",
        owner: "",
        arity: 1,
        params: &["bytes"],
        native: sum_native,
        fast: None,
        flags: 0,
    };

    fn fill_native(ctx: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
        let cx = ArgCx::new(ctx, &this, args, &FILL, OP_CTX_FLAG);
        let a: &mut [u8] = cx.arg(0)?;
        let b: &[u8] = cx.arg(1)?;
        let n = cx.with_ctx(|c| {
            // Re-entrant JS sees the lent buffer as detached.
            let g = c.global_object();
            let probe = c.get_member(&g, "probe").map_err(abrupt_value)?;
            c.invoke(probe, Value::Undefined, &[])
        })?;
        for (x, y) in a.iter_mut().zip(b) {
            *x = *y;
        }
        cx.ret(n)
    }
    const OP_CTX_FLAG: u32 = private::OP_CTX;
    static FILL: OpDesc = OpDesc {
        name: "fill",
        owner: "",
        arity: 2,
        params: &["dst", "src"],
        native: fill_native,
        fast: None,
        flags: OP_CTX_FLAG,
    };

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
                format!("threw {}", m.map(|m| match m {
                    Value::Str(s) => s.to_string(),
                    _ => "?".into(),
                }).unwrap_or_default())
            }
        }
    }

    #[test]
    fn byte_views_borrow_alias_and_lend() {
        let mut e = crate::Engine::new();
        e.define_op(&SUM);
        e.define_op(&FILL);
        assert_eq!(run(&mut e, "sum(new Uint8Array([1,2,3]))"), "6");
        assert_eq!(run(&mut e, "sum(new Uint8Array([1,2,3,4]).subarray(1,3))"), "5");
        assert_eq!(run(&mut e, "sum(new Uint16Array([257]).buffer)"), "2");
        assert_eq!(run(&mut e, "sum(new DataView(new Uint8Array([9,9,9]).buffer, 1))"), "18");
        let r = run(&mut e, "sum(5)");
        assert!(r.contains("sum: argument 1 (bytes) must be an ArrayBuffer"), "{r}");
        let r = run(&mut e, "var u = new Uint8Array(4); u.buffer.transfer(); sum(u)");
        assert!(r.contains("detached"), "{r}");
        // Aliasing: dst overlaps src.
        let r = run(&mut e, "var q = new Uint8Array(8); fill(q, q.subarray(2))");
        assert!(r.contains("argument 2 (src) overlaps argument 1 (dst)"), "{r}");
        // Disjoint halves of one buffer are fine; re-entrant JS sees them detached.
        let r = run(
            &mut e,
            "var w = new Uint8Array([0,0,7,8]); var seen; \
             globalThis.probe = () => { seen = w.length; return 1; }; \
             fill(w.subarray(0,2), w.subarray(2)); `${seen} ${w.length} ${w[0]} ${w[1]}`",
        );
        assert_eq!(r, "0 4 7 8");
        let r = run(&mut e, "Array.from(new Uint8Array([1,2,3]).map(x => x)).join()");
        assert_eq!(r, "1,2,3");
        // Vec<u8> results adopt the vector.
        let v = uint8array_from_vec(e.ctx(), vec![4, 5, 6]);
        e.ctx()
            .global
            .borrow_mut()
            .props
            .insert("made", Property::builtin(v));
        assert_eq!(run(&mut e, "made instanceof Uint8Array && made.join()"), "4,5,6");
    }
}
