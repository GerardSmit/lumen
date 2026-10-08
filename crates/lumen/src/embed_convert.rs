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
//! Aliasing is checked per call: a `&mut [u8]` overlapping any other slice argument from the same
//! backing is a `TypeError`. A native op without `&mut Ctx` borrows a SharedArrayBuffer in place
//! under its backing lock; a mutable-context op uses a snapshot and locked writeback because it
//! may re-enter JS, which must be able to access that buffer while the op runs. Immutable buffers
//! reject `&mut [u8]`.
//!
//! # Names
//! Free functions keep their Rust name; class members are camelCased (`set_x` sets `x`);
//! `name = ".."` / `rename(js = "..")` override both. A function's `length` is its number of
//! required parameters. Protocols: `iter` is `[Symbol.iterator]`, `next` returns
//! `{ value, done }`, `str` is `toString`, `len` is a `length` getter; the others are not
//! installed in JS.

use crate::fasthash::FastMap;
use crate::interpreter::{Abrupt, Interp, abrupt_value};
use crate::lstr::LStr;
use crate::value::{
    Callable, Exotic, Gc, NativeFn, Object, Property, TaInfo, TaKind, Value, WeakGc,
};
pub use lumen_bind::Slot;
use lumen_bind::{
    BigInt, Class, Elem, FnDesc, FnItem, FromArg, Host, IntKind, IntoError, IntoRet, Make, Methods,
    Module, ModuleItems, Native, NextRet, Owner, Role, ScalarEntry, SpawnHost, State, StateHost,
    flags,
};
use lumen_common::buffer::StoreSlot;
use lumen_common::native::{Data, NativeError};
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
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
    shared_id: Option<u64>,
    base: *mut u8,
    off: usize,
    len: usize,
    mutable: bool,
    slot: Slot,
}

struct SharedWriteback {
    id: u64,
    offset: usize,
    before: Box<[u8]>,
    bytes: Box<[u8]>,
}

/// A shared-byte mutex lease whose owner Arc outlives the extended guard lifetime.
struct SharedByteGuard {
    guard: Option<std::sync::MutexGuard<'static, Vec<u8>>>,
    _memory: crate::interpreter::SharedMem,
}

impl SharedByteGuard {
    fn new(memory: crate::interpreter::SharedMem) -> Self {
        let guard = memory.lock().unwrap();
        // SAFETY: `_memory` retains the mutex allocation until Drop, and Drop releases `guard`
        // explicitly before `_memory` is dropped. All accesses use the held mutex lease.
        let guard = unsafe {
            std::mem::transmute::<
                std::sync::MutexGuard<'_, Vec<u8>>,
                std::sync::MutexGuard<'static, Vec<u8>>,
            >(guard)
        };
        Self {
            guard: Some(guard),
            _memory: memory,
        }
    }

    fn len(&self) -> usize {
        self.guard.as_ref().unwrap().len()
    }

    fn as_mut_ptr(&self) -> *mut u8 {
        self.guard.as_ref().unwrap().as_ptr() as *mut u8
    }
}

impl Drop for SharedByteGuard {
    fn drop(&mut self) {
        self.guard.take();
    }
}

const INLINE_BORROWS: usize = 4;

#[derive(Default)]
pub(crate) struct Scratch {
    more_borrows: Vec<ByteBorrow>,
    lent: Vec<(usize, StoreSlot)>,
    strs: Vec<LStr>,
    vals: Vec<Box<[Value]>>,
    copies: Vec<Box<[u8]>>,
    shared_writes: Vec<SharedWriteback>,
    shared_guards: Vec<(u64, SharedByteGuard)>,
    guards: Vec<ClassBorrowGuard>,
    new_target: Option<Value>,
}

const UNDEF: &Value = &Value::Undefined;

/// Native operation scopes are installed by an embedder, never looked up in
/// author globals. The shared binding thunk enters after conversion and exits
/// after releasing borrowed native receivers, including on abrupt completion.
#[derive(Clone, Copy)]
pub struct NativeOperationHooks {
    pub begin: fn(&mut Interp) -> Result<(), Value>,
    pub end: fn(&mut Interp),
    /// Rust unwind cleanup only: must not execute JavaScript.
    pub abort: fn(&mut Interp),
}

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
    // Most typed operations borrow just their receiver. Keep the erased
    // RefCell guard inline rather than allocating for every property access.
    first_guard: UnsafeCell<Option<ClassBorrowGuard>>,
    operation_end: Cell<Option<fn(&mut Interp)>>,
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
            first_guard: UnsafeCell::new(None),
            operation_end: Cell::new(None),
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

    fn retain_class_guard(&self, guard: ClassBorrowGuard) {
        // SAFETY: single-threaded operation context; callers retain references
        // into the retained RefCell allocation, never this slot. Its contents are
        // dropped only after the bound operation and result conversion finish.
        let first = unsafe { &mut *self.first_guard.get() };
        if first.is_none() { *first = Some(guard); }
        else { self.scratch().guards.push(guard); }
    }

    /// Lock every SharedArrayBuffer argument in the same global order used by WebAssembly. This
    /// lets a no-context native op borrow several shared slices directly without deadlocking an
    /// overlapping multi-buffer op running in another agent.
    fn lock_shared_arg_buffers(&self) {
        if self
            .scratch_ref()
            .is_some_and(|scratch| !scratch.shared_guards.is_empty())
        {
            return;
        }
        let interp = self.interp();
        let mut memories = std::iter::once(self.this)
            .chain(self.args.iter())
            .filter_map(|value| {
                let (key, _, _) = resolve_view(interp, value).ok()?;
                let id = *interp.shared_buffers.get(&key)?;
                let memory = crate::interpreter::shared_mem_get(id)?;
                Some((std::sync::Arc::as_ptr(&memory) as usize, id, memory))
            })
            .collect::<Vec<_>>();
        memories.sort_by_key(|(key, _, _)| *key);
        memories.dedup_by(|left, right| left.0 == right.0);
        let guards = memories
            .into_iter()
            .map(|(_, id, memory)| (id, SharedByteGuard::new(memory)))
            .collect::<Vec<_>>();
        self.scratch().shared_guards.extend(guards);
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
        if !self.scratch.get().is_null() {
            // For a no-context op this runs after its native body has finished, before result
            // conversion can execute JavaScript. Context-taking ops use snapshots instead.
            self.scratch().shared_guards.clear();
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
        // Shared byte views handed to native code point into call-local snapshots. They do not
        // need a ByteStore lend, and their backing remains available to JS under its mutex.
        if ctx.shared_buffers.contains_key(&key) {
            return None;
        }
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
            let end = off
                .checked_add(len)
                .ok_or_else(|| self.type_error(at, "SharedArrayBuffer view is out of range"))?;
            for k in 0..self.nborrow.get() {
                let b = self.borrow_at(k);
                if b.key != key && b.shared_id != Some(id) {
                    continue;
                }
                let overlap = len != 0 && b.len != 0 && off < b.off + b.len && b.off < off + len;
                if overlap && (mutable || b.mutable) {
                    let msg = format!(
                        "overlaps {} in the same buffer; a mutable byte slice cannot alias",
                        self.slot_label(b.slot)
                    );
                    return Err(self.type_error(at, &msg));
                }
            }
            let mem = crate::interpreter::shared_mem_get(id)
                .ok_or_else(|| self.type_error(at, "SharedArrayBuffer memory is gone"))?;
            if self.desc.flags & flags::CTX == 0 {
                // Slice arguments are converted after scriptful arguments. Without a mutable
                // context the native body cannot re-enter JS, so keep the backing mutex for the
                // call and hand it a direct pointer. Reuse one lease for disjoint views of the
                // same shared backing rather than recursively locking its mutex.
                self.lock_shared_arg_buffers();
                let existing = self.scratch_ref().and_then(|scratch| {
                    scratch
                        .shared_guards
                        .iter()
                        .find(|(guard_id, _)| *guard_id == id)
                        .map(|(_, guard)| (guard.as_mut_ptr(), guard.len()))
                });
                let base = if let Some((base, visible)) = existing {
                    if end > visible {
                        return Err(self.type_error(
                            at,
                            "view is out of range of its SharedArrayBuffer",
                        ));
                    }
                    base
                } else {
                    let guard = SharedByteGuard::new(mem);
                    if end > guard.len() {
                        return Err(self.type_error(
                            at,
                            "view is out of range of its SharedArrayBuffer",
                        ));
                    }
                    let base = guard.as_mut_ptr();
                    self.scratch().shared_guards.push((id, guard));
                    base
                };
                self.push_borrow(ByteBorrow {
                    key,
                    shared_id: Some(id),
                    base,
                    off,
                    len,
                    mutable,
                    slot: at,
                });
                // SAFETY: `base` is protected by the call's retained mutex guard, and the
                // checked range is within that backing.
                return Ok((unsafe { base.add(off) }, len));
            }

            // A context-taking native op may call back into JS, which needs this mutex. Keep a
            // call-local copy instead; mutable changes are merged under the mutex on return.
            let (before, copy) = {
                let m = mem.lock().unwrap();
                let Some(bytes) = m.get(off..end) else {
                    return Err(
                        self.type_error(at, "view is out of range of its SharedArrayBuffer")
                    );
                };
                let copy: Box<[u8]> = bytes.into();
                (mutable.then(|| copy.clone()), copy)
            };
            let s = self.scratch();
            let (base, copy_len) = if mutable {
                s.shared_writes.push(SharedWriteback {
                    id,
                    offset: off,
                    before: before.unwrap(),
                    bytes: copy,
                });
                let write = s.shared_writes.last_mut().unwrap();
                (write.bytes.as_mut_ptr(), write.bytes.len())
            } else {
                s.copies.push(copy);
                let copy = s.copies.last_mut().unwrap();
                (copy.as_mut_ptr(), copy.len())
            };
            self.push_borrow(ByteBorrow {
                key,
                shared_id: Some(id),
                base,
                off,
                len: copy_len,
                mutable,
                slot: at,
            });
            return Ok((base, copy_len));
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
            if b.key != key || b.shared_id.is_some() {
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
            shared_id: None,
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

    /// The error for a required argument that was not passed: `TypeError: <op>: argument N
    /// (<name>) is required`, or the op's own `hint(js(missing_message = "..", missing_code =
    /// ".."))` wording and `err.code`.
    #[cold]
    fn missing_argument(&self, at: Slot) -> Value {
        let Some(message) = self.desc.hint("js", "missing_message") else {
            return self.type_error(at, "is required");
        };
        let error = self.interp().make_error("TypeError", message.to_string());
        if let (Value::Obj(object), Some(code)) = (&error, self.desc.hint("js", "missing_code")) {
            object
                .borrow_mut()
                .props
                .insert("code", Property::plain(Value::str(code)));
        }
        error
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
        let Some((_, class_proto)) = resolved_class::<T>(ctx) else {
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
            None if at == Slot::THIS && T::DESC.hint("js", "invalid_this").is_some() => {
                let msg = format!("Value of \"this\" must be of type {}", class_name::<T>());
                let error = self.interp().make_error("TypeError", msg);
                if let Value::Obj(object) = &error {
                    object
                        .borrow_mut()
                        .props
                        .insert("code", Property::plain(Value::str("ERR_INVALID_THIS")));
                }
                Err(error)
            }
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
                let value = unsafe { &*ptr }
                    .downcast_ref::<T>()
                    .expect("matching class view");
                self.retain_class_guard(guard);
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
        let r = std::cell::Ref::map(r, |value| value as &dyn Any);
        self.retain_class_guard(ClassBorrowGuard::Shared {
            borrow: Some(r),
            owner: rc,
        });
        Ok(unsafe { &*p })
    }

    /// Borrow a class instance exclusively (`&mut self` receivers, `&mut T` arguments). A
    /// conflicting borrow (re-entrant call on the same instance) throws a TypeError.
    #[allow(clippy::mut_from_ref)]
    pub fn class_mut<'a, T: Class>(&'a self, v: &Value, at: Slot) -> Result<&'a mut T, Value> {
        if let Some(entry) = host_entry(self.interp(), v) {
            if let Ok((ptr, guard)) = (entry.view_mut)(&entry.data, TypeId::of::<T>()) {
                let value = unsafe { &mut *ptr }
                    .downcast_mut::<T>()
                    .expect("matching class view");
                self.retain_class_guard(guard);
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
        let r = std::cell::RefMut::map(r, |value| value as &mut dyn Any);
        self.retain_class_guard(ClassBorrowGuard::Exclusive {
            borrow: Some(r),
            owner: rc,
        });
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

// A mapped Ref retains the original RefCell borrow even for an inherited
// native projection. Its owner keeps that RefCell allocated until release.
// Neither reference points into this movable enum or the guard vector.
enum ClassBorrowGuard {
    Shared { borrow: Option<std::cell::Ref<'static, dyn Any>>, owner: Rc<dyn Any> },
    Exclusive { borrow: Option<std::cell::RefMut<'static, dyn Any>>, owner: Rc<dyn Any> },
}
impl Drop for ClassBorrowGuard {
    fn drop(&mut self) {
        match self {
            Self::Shared { borrow, owner } => { borrow.take(); let _ = owner; }
            Self::Exclusive { borrow, owner } => { borrow.take(); let _ = owner; }
        }
    }
}

impl Drop for ArgCx<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        // Fast path: plain-value ops never allocate scratch.
        if !self.scratch.get().is_null() {
            self.drop_scratch();
        } else {
            self.first_guard.get_mut().take();
        }
        if self.operation_end.take().is_some() {
            // A panic escaped Native::call. Native receiver guards have been
            // released; restore embedder scope state without running author JS.
            let ctx = unsafe { &mut *self.interp };
            if let Some(hooks) = ctx.op_state().get::<NativeOperationHooks>().copied() {
                (hooks.abort)(ctx);
            }
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
            for write in &s.shared_writes {
                let Some(memory) = crate::interpreter::shared_mem_get(write.id) else {
                    continue;
                };
                let Ok(mut bytes) = memory.lock() else {
                    continue;
                };
                let Some(dst) = bytes.get_mut(write.offset..write.offset + write.bytes.len())
                else {
                    continue;
                };
                for ((dst, before), value) in dst
                    .iter_mut()
                    .zip(&write.before)
                    .zip(&write.bytes)
                {
                    if before != value {
                        *dst = *value;
                    }
                }
            }
            self.first_guard.get_mut().take();
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

impl Interp {
    /// WebIDL signed long conversion using the binding engine's canonical
    /// ToNumber/ToInt32 machinery, for algorithms with staged conversion order.
    pub fn webidl_long(&mut self, value: &Value) -> Result<i32, Value> {
        self.coerce_number(value).map(crate::eval::to_int32)
    }
    /// Convert an iterable through the engine's iterator protocol. Each item is
    /// converted before advancing; abrupt conversion closes the iterator while
    /// preserving the original exception. Non-iterable array-like objects fail.
    pub fn convert_iterable<T>(
        &mut self,
        value: &Value,
        max_items: usize,
        convert: impl FnMut(&mut Self, Value) -> OpResult<T>,
    ) -> OpResult<Vec<T>> {
        let (iterator, next) = self
            .get_iterator(value)
            .map_err(|error| OpError::thrown(abrupt_value(error)))?;
        self.convert_iterator(iterator,next,max_items,convert)
    }

    /// Convert a Web IDL union's already selected iterator method exactly once.
    pub fn convert_iterable_with_method<T>(&mut self,value:&Value,method:Value,max_items:usize,
        convert:impl FnMut(&mut Self,Value)->OpResult<T>)->OpResult<Vec<T>> {
        let (iterator,next)=self.get_iterator_from_method(value,method)
            .map_err(|error|OpError::thrown(abrupt_value(error)))?;
        self.convert_iterator(iterator,next,max_items,convert)
    }

    fn convert_iterator<T>(&mut self,iterator:Value,next:Value,max_items:usize,
        mut convert:impl FnMut(&mut Self,Value)->OpResult<T>)->OpResult<Vec<T>> {
        let mut values = Vec::new();
        loop {
            self.poll_native(values.len())
                .map_err(|error| OpError::thrown(abrupt_value(error)))?;
            let Some(value) = self
                .iterator_step(&iterator, &next)
                .map_err(|error| OpError::thrown(abrupt_value(error)))?
            else {
                return Ok(values);
            };
            let step = (|| {
                if values.len() >= max_items {
                    return Err(OpError::new(
                        "RangeError",
                        "iterable exceeds the host limit",
                    ));
                }
                self.check_grow(&values)
                    .map_err(|error| OpError::thrown(abrupt_value(error)))?;
                convert(self, value)
            })();
            match step {
                Ok(value) => values.push(value),
                Err(error) => {
                    self.iterator_close(&iterator);
                    return Err(error);
                }
            }
        }
    }

    /// Collect genuine iterable values without consulting an author-modified
    /// Array constructor or accepting the array-like fallback of Array.from.
    pub fn iterable_to_list(&mut self, value: &Value, max_items: usize) -> OpResult<Vec<Value>> {
        self.convert_iterable(value, max_items, |_, value| Ok(value))
    }

    /// The intrinsic `Reflect.ownKeys(target)`: string and symbol keys, in spec order, without
    /// consulting the author-visible `Reflect`.
    pub fn reflect_own_keys(&mut self, target: &Value) -> Result<Vec<Value>, Value> {
        let keys = crate::builtins::reflect::reflect_own_keys(
            self,
            Value::Undefined,
            std::slice::from_ref(target),
        )?;
        let length = match self.member_get(&keys, "length")? {
            Value::Num(length) if length >= 0.0 => length as usize,
            _ => 0,
        };
        (0..length)
            .map(|index| self.member_get(&keys, &index.to_string()))
            .collect()
    }

    /// The well-known symbol `Symbol.<name>` (`"iterator"`, `"toStringTag"`, ...).
    pub fn well_known_symbol(&mut self, name: &str) -> Option<Value> {
        let key = crate::builtins::well_known_key(self, name)?;
        self.sym_from_key(&key)
    }

    /// `Symbol.for(key)`: the registry symbol, created on first use.
    pub fn symbol_for(&mut self, key: &str) -> Value {
        let data = match crate::interpreter::sym_for_get(key) {
            Some(data) => data,
            None => {
                let symbol = self.new_symbol(Some(Rc::from(key)));
                let Value::Sym(data) = symbol else {
                    unreachable!("new_symbol must return a symbol")
                };
                crate::interpreter::sym_for_insert(key.to_string(), data.clone());
                data
            }
        };
        Value::Sym(data)
    }

    /// PromiseResolve with this realm's intrinsic Promise constructor.
    /// Preserves a genuine promise's identity and observable constructor errors.
    pub fn coerce_promise(&mut self, value: Value) -> OpResult<Value> {
        self.promise_resolve_checked(value).map_err(OpError::thrown)
    }

    /// Copy the current bytes of a genuine, non-shared BufferSource.
    /// Uses internal view brands and ranges, without reading author properties.
    /// Detached, out-of-bounds, shared and non-buffer objects return `None`.
    pub fn buffer_source_bytes(&self, value: &Value) -> Option<Vec<u8>> {
        let (buffer, offset, length) = resolve_view(self, value).ok()?;
        if self.shared_buffers.contains_key(&buffer) {
            return None;
        }
        let end = offset.checked_add(length)?;
        self.with_buffer_bytes(buffer, |bytes| bytes.get(offset..end).map(<[u8]>::to_vec))?
    }

    /// Read the bytes of any BufferSource (ArrayBuffer, SharedArrayBuffer, typed array or
    /// DataView) through internal brands and ranges, without copying and without reading author
    /// properties. A detached or out-of-bounds source reads as empty; `None` when `value` is not
    /// a BufferSource. The callback must not run JavaScript.
    pub fn with_buffer_source_bytes<R>(
        &self,
        value: &Value,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        let (buffer, offset, length) = match resolve_view(self, value) {
            Ok(range) => range,
            Err(ViewErr::NotView) => return None,
            Err(ViewErr::Detached) => return Some(read(&[])),
        };
        let mut read = Some(read);
        let found = offset.checked_add(length).and_then(|end| {
            self.with_buffer_bytes(buffer, |bytes| {
                bytes
                    .get(offset..end)
                    .map(|bytes| (read.take().expect("read once"))(bytes))
            })
            .flatten()
        });
        Some(match found {
            Some(result) => result,
            None => (read.take().expect("read once"))(&[]),
        })
    }

    /// Allocate a unique engine-private name for a native object's traced slot.
    ///
    /// The returned name is intentionally held only by the native payload. The slot is hidden
    /// from ordinary own-key reflection and its value is retained by the object's normal GC
    /// property graph, unlike Values stored in an opaque HostEntry payload.
    pub fn allocate_native_private_slot_name(&mut self) -> String {
        self.accessor_seq += 1;
        format!("#\u{0}native_slot_{}", self.accessor_seq)
    }

    fn native_private_slot_owner(
        &mut self,
        target: &Value,
        key: &str,
        require_extensible: bool,
    ) -> Result<crate::value::Gc, Value> {
        if !Self::is_private_key(key) {
            return Err(self.make_error("TypeError", "native slot key is not private"));
        }
        let Some(target) = target.as_obj().cloned() else {
            return Err(self.make_error("TypeError", "native slot owner must be an object"));
        };
        {
            let target = target.borrow();
            if require_extensible && !target.extensible || target.props.get(key).is_some() {
                return Err(self.make_error("TypeError", "native slot cannot be installed"));
            }
        }

        Ok(target)
    }

    /// Retain one value through an immutable, reflection-hidden own property.
    /// This supplies a normal traced GC edge without a backing-array allocation
    /// or an opaque native payload that permanently roots a cycle.
    pub fn define_native_private_value_slot(
        &mut self,
        target: &Value,
        key: &str,
        value: Value,
    ) -> Result<(), Value> {
        let target = self.native_private_slot_owner(target, key, true)?;
        target.borrow_mut().props.insert(key, Property::data(value, false, false, false));
        Ok(())
    }

    /// Store an internal slot whose initialization is independent of ordinary
    /// property extensibility (for example HTMLElement's attached internals).
    pub fn define_native_internal_value_slot(&mut self, target: &Value, key: &str, value: Value) -> Result<(), Value> {
        let target = self.native_private_slot_owner(target, key, false)?;
        target.borrow_mut().props.insert(key, Property::data(value, false, false, false));
        Ok(())
    }

    /// Update mutable internal state through a traced, reflection-hidden edge.
    /// Internal slots remain writable by the host after Object.freeze; author
    /// accessors, extensibility and property descriptors are never consulted.
    pub fn set_native_internal_value_slot(&mut self, target: &Value, key: &str, value: Value) -> Result<(), Value> {
        if !Self::is_private_key(key) { return Err(self.make_error("TypeError", "native slot key is not private")); }
        let Some(target) = target.as_obj().cloned() else { return Err(self.make_error("TypeError", "native slot owner must be an object")); };
        target.borrow_mut().props.insert(key, Property::data(value, false, false, false));
        Ok(())
    }

    /// Freeze an object in place: non-extensible, every own property non-writable and
    /// non-configurable (`Object.freeze` without consulting globals or running author code).
    pub fn freeze_native_object(&self, value: &Value) {
        self.freeze_object(value);
    }

    /// Store an immutable, reflection-hidden array on a native wrapper. Both the slot and its
    /// backing array are frozen using engine-owned object storage, without consulting globals or
    /// running author code. The array's elements therefore become ordinary traced GC edges.
    pub fn define_native_private_array_slot(
        &mut self,
        target: &Value,
        key: &str,
        values: Vec<Value>,
    ) -> Result<(), Value> {
        let target = self.native_private_slot_owner(target, key, true)?;
        let backing = self.make_array(values);
        self.freeze_object(&backing);
        target
            .borrow_mut()
            .props
            .insert(key, Property::data(backing, false, false, false));
        Ok(())
    }

    /// Read a traced native value directly without invoking author properties,
    /// getters, proxies, or reflection hooks.
    pub fn native_private_value_slot(&self, target: &Value, key: &str) -> Option<Value> {
        if !Self::is_private_key(key) {
            return None;
        }
        let target = target.as_obj()?.borrow();
        let slot = target.props.get(key)?;
        (!slot.accessor()).then(|| slot.value())
    }

    /// Read a native private array slot directly from the own data-property and packed array
    /// storage. This deliberately bypasses `Get`, iterators, proxies, and author-modified
    /// intrinsics; callers receive cloned Values so they can construct a fresh public result
    /// array while preserving the identities retained by the immutable backing array.
    pub fn native_private_array_slot(&self, target: &Value, key: &str) -> Option<Vec<Value>> {
        let backing = self.native_private_value_slot(target, key)?;
        let backing = backing.as_obj()?;
        if !matches!(&backing.borrow().exotic, Exotic::Array) {
            return None;
        }
        let length = self.array_length(backing);
        let backing = backing.borrow();
        (0..length)
            .map(|index| {
                u32::try_from(index)
                    .ok()
                    .and_then(|index| backing.props.get_index(index))
                    .filter(|property| !property.accessor())
                    .map(|property| property.value())
            })
            .collect()
    }
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
    let operation_end = cx.operation_end.take();
    drop(cx);
    if let Some(end) = operation_end { end(ctx); }
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
    let operation_end = cx.operation_end.take();
    drop(cx);
    if let Some(end) = operation_end { end(ctx); }
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

    fn begin_operation(cx: &ArgCx<'_>) -> Result<(), Value> {
        cx.with_ctx(|ctx| {
            let hooks = ctx.op_state().get::<NativeOperationHooks>().copied();
            if let Some(hooks) = hooks {
                (hooks.begin)(ctx)?;
                cx.operation_end.set(Some(hooks.end));
            }
            Ok(())
        })
    }

    fn validate_receiver<T: Class>(cx: &ArgCx<'_>, value: &Value) -> Result<(), Value> {
        cx.with_ctx(|ctx| ctx.with_instance::<T, _>(value, |_| ())
            .map_err(|error| error.to_value(ctx)))
    }

    /// JS has no keywords: named parameters are the arguments in declaration order. For optional
    /// parameters, `undefined` means omitted so defaults apply. Required parameters preserve it
    /// as a supplied value for Web IDL coercion; `Passed<T>` also distinguishes it from omission.
    #[inline(always)]
    fn bind<'c, const N: usize>(cx: &'c ArgCx<'_>) -> Result<[Option<&'c Value>; N], Value> {
        let args: &'c [Value] = cx.args;
        let max = cx.desc.max_pos as usize;
        let min = cx.desc.min_pos as usize;
        // Attribute setters convert an omitted first argument from undefined.
        // Their required value slot is not an operation's arity check.
        if args.len() < min && !matches!(cx.desc.role, Role::Setter) {
            return Err(cx.missing_argument(Slot::arg(args.len() as u32)));
        }
        // Keyword-only parameters after `*args` cannot be passed positionally.
        let positional = if N > max && cx.desc.has_varargs() {
            max
        } else {
            N
        };
        Ok(std::array::from_fn(|i| match args.get(i) {
            Some(value @ Value::Undefined)
                if i < positional
                    && cx.desc.params.get(i).is_some_and(|parameter| {
                        parameter.pass_undefined || !parameter.optional()
                    }) =>
            {
                Some(value)
            }
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
    fn is_true(v: &Value) -> bool {
        matches!(v, Value::Bool(true))
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

    /// `Option::None` is `undefined`; return [`Nullable`] for a JS `null`.
    #[inline(always)]
    fn none(_: &mut Interp) -> Value {
        Value::Undefined
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
        match resolved_class::<T>(ctx) {
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
        target.borrow_mut().props.insert(
            &*js_name(f.desc),
            Property::data(v, true, f.desc.hint("js", "webidl").is_some(), true),
        );
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
            .insert(k.name, Property::data(v, true, k.enumerable, true));
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

/// A nullable result (WebIDL `T?`): `None` is JS `null`, where a bare `Option<T>` is `undefined`.
#[derive(Clone, Debug, Default)]
pub struct Nullable<T>(pub Option<T>);

impl<T: IntoRet<JsHost>> IntoRet<JsHost> for Nullable<T> {
    const MAY_RUN: bool = T::MAY_RUN;
    #[inline]
    fn into_ret(self, ctx: &mut Interp) -> Result<Value, Value> {
        match self.0 {
            Some(value) => value.into_ret(ctx),
            None => Ok(Value::Null),
        }
    }
}

impl<T: Elem> Elem for Nullable<T> {}

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
        props: Vec<(Cow<'static, str>, Data)>,
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
            props: Vec::new(),
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
    /// Attach an own property to the error object (`err.errno`, `err.syscall`, ...).
    pub fn with_prop(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<Data>,
    ) -> OpError {
        if let OpErrorRepr::New { props, .. } = &mut *self.0 {
            props.push((name.into(), value.into()));
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
                props,
            } => {
                let message = message.into_owned();
                let dom = matches!(
                    class,
                    "InvalidStateError"
                        | "NotFoundError"
                        | "HierarchyRequestError"
                        | "QuotaExceededError"
                        | "NotSupportedError"
                        | "WrongDocumentError"
                        | "InvalidCharacterError"
                        | "NoModificationAllowedError"
                        | "NamespaceError"
                        | "NotAllowedError"
                        | "AbortError"
                        | "SecurityError"
                        | "NetworkError"
                        | "TimeoutError"
                        | "UnknownError"
                        | "IndexSizeError"
                        | "InvalidAccessError"
                        | "DataError"
                        | "OperationError"
                        | "InvalidNodeTypeError"
                        | "DataCloneError"
                );
                let e = if dom {
                    let global = ctx.global_object();
                    match ctx.get_member(&global, "DOMException") {
                        Ok(constructor) if constructor.is_callable() => ctx
                            .construct_value(
                                constructor,
                                &[Value::str(&message), Value::str(class)],
                            )
                            .unwrap_or_else(|error| error),
                        _ => {
                            let error = ctx.make_error(class, message);
                            if let Value::Obj(object) = &error {
                                object
                                    .borrow_mut()
                                    .props
                                    .insert("name", Property::builtin(Value::str(class)));
                            }
                            error
                        }
                    }
                } else {
                    ctx.make_error(class, message)
                };
                if let Value::Obj(o) = &e {
                    if let Some(code) = code {
                        o.borrow_mut()
                            .props
                            .insert("code", Property::plain(Value::str(&*code)));
                    }
                    for (name, data) in props {
                        if let Ok(v) = <Data as IntoRet<JsHost>>::into_ret(data, ctx) {
                            o.borrow_mut().props.insert(&*name, Property::plain(v));
                        }
                    }
                }
                e
            }
        }
    }
}

struct DeferredMicrotasks(Rc<RefCell<Vec<Value>>>);

pub(crate) fn flush_deferred_microtasks(ctx: &mut Interp) {
    let pending = ctx
        .host_state
        .get::<DeferredMicrotasks>()
        .map(|queue| std::mem::take(&mut *queue.0.borrow_mut()))
        .unwrap_or_default();
    for callback in pending {
        ctx.queue_microtask(callback);
    }
}

impl Interp {
    /// Native mutation sinks can enqueue jobs without borrowing the interpreter.
    pub fn deferred_microtasks(&mut self) -> Rc<RefCell<Vec<Value>>> {
        if !self.host_state.has::<DeferredMicrotasks>() {
            self.host_state
                .put(DeferredMicrotasks(Rc::new(RefCell::new(Vec::new()))));
        }
        self.host_state
            .get::<DeferredMicrotasks>()
            .unwrap()
            .0
            .clone()
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
    /// Publish host identity before the Promise init hook can reenter the host.
    /// The registration callback must only update host metadata, without invoking
    /// author code. No host borrow may survive its return.
    pub fn new_registered<R>(
        ctx: &mut Interp,
        register: impl FnOnce(&mut Interp, Deferred) -> R,
    ) -> R {
        ctx.new_promise_registered(|ctx, promise| register(ctx, Deferred { promise }))
    }
    /// The promise JS sees.
    pub fn promise(&self) -> Value {
        self.promise.clone()
    }
    /// Borrow the stored promise for native-owner GC edge enumeration.
    pub fn promise_value(&self) -> &Value {
        &self.promise
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
    /// Reject with the original reason and mark this promise internally handled.
    /// Used by specifications that explicitly set [[PromiseIsHandled]], without
    /// creating a reaction, dependent promise, or author-observable callback.
    pub fn reject_handled(self, ctx: &mut Interp, reason: Value) {
        ctx.reject_promise(&self.promise, reason);
        let marked = ctx.mark_promise_handled(&self.promise);
        debug_assert!(marked, "a Deferred holds an intrinsic promise");
    }
    /// The standard `(resolve, reject)` JS function pair — what a callback-based host API
    /// expects (e.g. `lumen_host::TaskRegistry::register(resolve, Some(reject), decode)`).
    pub fn resolving_functions(&self, ctx: &mut Interp) -> (Value, Value) {
        ctx.make_resolver_pair(&self.promise)
    }
    /// Observe a native promise's rejection as part of this deferred
    /// operation. Uses the pristine engine reaction machinery, so replacing
    /// Promise.prototype.then cannot intercept the host's internal observer.
    pub fn reject_on(&self, ctx: &mut Interp, source: &Value) {
        let (_, reject) = self.resolving_functions(ctx);
        ctx.promise_then(source, Value::Undefined, reject);
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
    pub props: Vec<(Cow<'static, str>, Data)>,
}

impl SendError {
    pub fn new(class: &'static str, message: impl Into<String>) -> SendError {
        SendError {
            class,
            message: message.into(),
            code: None,
            props: Vec::new(),
        }
    }
    pub fn with_code(mut self, c: impl Into<Cow<'static, str>>) -> SendError {
        self.code = Some(c.into());
        self
    }
}

impl From<SendError> for OpError {
    fn from(e: SendError) -> OpError {
        let mut err = OpError::new(e.class, e.message);
        if let OpErrorRepr::New { props, .. } = &mut *err.0 {
            *props = e.props;
        }
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
        K::Named(class) => class,
    }
}

impl From<NativeError> for SendError {
    fn from(e: NativeError) -> SendError {
        SendError {
            class: native_error_class(e.kind),
            message: e.message.into_owned(),
            code: e.code,
            props: e.props,
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

/// Per-interpreter class table keyed by active realm and Rust class type.
#[derive(Default)]
struct ClassRegistry {
    map: HashMap<(RealmKey, TypeId), (Value, Gc)>,
    /// Classes of lazily installed modules not built yet, by realm and JS name, so a native
    /// value of such a class can be wrapped before (or without) its global being read.
    pending: HashMap<(RealmKey, &'static str), Make<JsHost>>,
}

/// A realm cache key owns the global whose address identifies that realm. Keeping a strong
/// handle in every cache key prevents address reuse from aliasing a later realm if realm-table
/// retention changes independently of these host caches.
#[derive(Clone)]
struct RealmKey(Gc);

impl PartialEq for RealmKey {
    fn eq(&self, other: &Self) -> bool {
        Gc::as_ptr(&self.0) == Gc::as_ptr(&other.0)
    }
}

impl Eq for RealmKey {}

impl Hash for RealmKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(&self.0).hash(state);
    }
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

/// Trace identities that remain observable through a reachable native owner.
///
/// Implementations must only inspect native state and upgrade weak identities;
/// they must not run author code, allocate JS objects, or retain emitted Values.
/// The epoch permits an owner to deduplicate shared native graph walks per GC.
pub trait NativeIdentityOwner: Class {
    fn trace_native_identities(&self, epoch: u64, visit: &mut dyn FnMut(&Value));

    /// Opt in only when every native-held JavaScript Value is enumerated by
    /// `trace_native_values`. Opaque callback owners retain their existing pins.
    const TRACES_NATIVE_VALUES: bool = false;

    /// Visit each actual stored Value reference once, without cloning it or
    /// invoking author code. These are owner edges, not independent GC roots.
    fn trace_native_values(&self, _visit: &mut dyn FnMut(&Value)) {}
}

struct HostEntry {
    _weak: WeakGc,
    data: Rc<dyn Any>,
    /// The exact engine-created Proxy identity used for this instance's indexed wrapper, if any.
    /// It grants native receiver branding only to that wrapper, never to author Proxies. The weak
    /// handle avoids retaining the wrapper or allowing a stale pointer key to alias a new object.
    indexed_wrapper: Option<WeakGc>,
    indexed_read: Option<IndexedRead>,
    indexed_index_of: Option<NativeFn>,
    retained: Option<Value>,
    identity: bool,
    callbacks_retained: bool,
    identity_owner: Option<fn(&HostEntry, u64, &mut dyn FnMut(&Value))>,
    native_values: Option<fn(&HostEntry, &mut dyn FnMut(&Value))>,
    view_ref: fn(&Rc<dyn Any>, TypeId) -> Result<(*const dyn Any, ClassBorrowGuard), ()>,
    view_mut: fn(&Rc<dyn Any>, TypeId) -> Result<(*mut dyn Any, ClassBorrowGuard), ()>,
}

struct IndexedRead {
    trap: WeakGc,
    item: WeakGc,
}

fn host_entry_key(i: &Interp, v: &Value) -> Option<usize> {
    let object = v.as_obj()?;
    let key = Gc::as_ptr(object) as usize;
    let objects = i.host_state.get::<HostObjects>()?;

    // A Proxy receiver inherits native data only when this exact identity was created by the
    // indexed-class adapter. Looking through arbitrary Proxy targets would let author code bypass
    // native/Web IDL receiver brand checks with `new Proxy(native, {})`.
    if let Some((target, _)) = i.proxies.get(&key) {
        let target = target.as_obj()?;
        let target_key = Gc::as_ptr(target) as usize;
        let entry = objects.map.get(&target_key)?;
        let trusted_wrapper = entry
            .indexed_wrapper
            .as_ref()
            .and_then(WeakGc::upgrade)
            .is_some_and(|registered| Gc::ptr_eq(&registered, object));
        return trusted_wrapper.then_some(target_key);
    }

    // A native Window receiver may be a WindowProxy only during the exact same-origin internal
    // Get/Set operation that authorized it. The operation scope carries a trusted caller/holder/
    // target tuple; arbitrary Proxies (including a Proxy around a WindowProxy) never enter here.
    #[cfg(feature = "embed")]
    if let Some(target_key) = crate::embed_realms::authorized_window_target(i, key) {
        return objects.map.contains_key(&target_key).then_some(target_key);
    }

    objects.map.contains_key(&key).then_some(key)
}

/// The checked projection path used by native accessors. Unlike identity caches and GC retention,
/// an actual native binding can preserve the host's thrown cross-origin error value.
fn host_entry_key_checked(i: &mut Interp, v: &Value) -> Result<Option<usize>, Value> {
    let Some(object) = v.as_obj() else {
        return Ok(None);
    };
    let key = Gc::as_ptr(object) as usize;
    if let Some((target, _)) = i.proxies.get(&key) {
        let Some(target) = target.as_obj() else {
            return Ok(None);
        };
        let target_key = Gc::as_ptr(target) as usize;
        let Some(entry) = i
            .host_state
            .get::<HostObjects>()
            .and_then(|objects| objects.map.get(&target_key))
        else {
            return Ok(None);
        };
        let trusted_wrapper = entry
            .indexed_wrapper
            .as_ref()
            .and_then(WeakGc::upgrade)
            .is_some_and(|registered| Gc::ptr_eq(&registered, object));
        return Ok(trusted_wrapper.then_some(target_key));
    }

    #[cfg(feature = "embed")]
    if let Some(target_key) = i.window_proxy_native_target(key)? {
        return Ok(i
            .host_state
            .get::<HostObjects>()
            .is_some_and(|objects| objects.map.contains_key(&target_key))
            .then_some(target_key));
    }

    Ok(i.host_state
        .get::<HostObjects>()
        .is_some_and(|objects| objects.map.contains_key(&key))
        .then_some(key))
}

fn host_entry<'a>(i: &'a Interp, v: &Value) -> Option<&'a HostEntry> {
    let key = host_entry_key(i, v)?;
    i.host_state.get::<HostObjects>()?.map.get(&key)
}

fn view_ref<T: Class>(
    data: &Rc<dyn Any>,
    ty: TypeId,
) -> Result<(*const dyn Any, ClassBorrowGuard), ()> {
    let rc = data.clone().downcast::<RefCell<T>>().map_err(|_| ())?;
    let borrow = rc.try_borrow().map_err(|_| ())?;
    let borrow = std::cell::Ref::filter_map(borrow, |value| value.view(ty)).map_err(|_| ())?;
    let ptr = &*borrow as *const dyn Any;
    // SAFETY: the erased owner retains the same RefCell allocation; Drop
    // releases its mapped borrow before releasing that owner.
    let borrow =
        unsafe { std::mem::transmute::<std::cell::Ref<'_, dyn Any>, std::cell::Ref<'static, dyn Any>>(borrow) };
    Ok((
        ptr,
        ClassBorrowGuard::Shared { borrow: Some(borrow), owner: rc },
    ))
}

fn view_mut<T: Class>(data: &Rc<dyn Any>, ty: TypeId) -> Result<(*mut dyn Any, ClassBorrowGuard), ()> {
    let rc = data.clone().downcast::<RefCell<T>>().map_err(|_| ())?;
    let borrow = rc.try_borrow_mut().map_err(|_| ())?;
    let mut borrow = std::cell::RefMut::filter_map(borrow, |value| value.view_mut(ty)).map_err(|_| ())?;
    let ptr = &mut *borrow as *mut dyn Any;
    // SAFETY: same owner/borrow lifetime and release order as view_ref.
    let borrow = unsafe {
        std::mem::transmute::<std::cell::RefMut<'_, dyn Any>, std::cell::RefMut<'static, dyn Any>>(borrow)
    };
    Ok((
        ptr,
        ClassBorrowGuard::Exclusive { borrow: Some(borrow), owner: rc },
    ))
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

#[inline]
fn active_realm_key(i: &Interp) -> RealmKey {
    RealmKey(i.global.clone())
}

fn new_instance<T: Class>(i: &mut Interp, value: T, proto: Gc) -> Value {
    let obj = Object::new(Some(proto));
    attach_instance(i, &obj, value);
    let target = Value::Obj(obj);
    let key = (active_realm_key(i), TypeId::of::<T>());
    let indexed = i
        .host_state
        .get::<IndexedClasses>()
        .and_then(|r| r.0.get(&key))
        .cloned();
    if let Some((handler, index_of)) = indexed {
        let wrapper = crate::builtins::proxy::make_proxy(i, target.clone(), handler)
            .unwrap_or_else(|_| panic!("native indexed wrapper"));
        register_indexed_wrapper(i, &target, &wrapper);
        let key = host_entry_key(i, &wrapper).expect("registered indexed wrapper");
        host_objects(i).map.get_mut(&key).expect("indexed host entry").indexed_index_of = index_of;
        wrapper
    } else {
        target
    }
}

fn register_indexed_wrapper(i: &mut Interp, target: &Value, wrapper: &Value) {
    let (Some(target), Some(wrapper)) = (target.as_obj(), wrapper.as_obj()) else {
        panic!("indexed native wrapper must be an object");
    };
    let target_key = Gc::as_ptr(target) as usize;
    let wrapper_key = Gc::as_ptr(wrapper) as usize;
    let Some((proxy_target, handler)) = i.proxies.get(&wrapper_key) else {
        panic!("indexed native wrapper must be an engine Proxy");
    };
    if !matches!(proxy_target.as_obj(), Some(proxy_target) if Gc::ptr_eq(proxy_target, target)) {
        panic!("indexed native wrapper target mismatch");
    }
    let indexed_read = handler.as_obj().and_then(|handler| {
        let trap = handler.borrow().props.get("get")?.value().as_obj()?.clone();
        let item = match &trap.borrow().call {
            Callable::Bound(bound) if matches!(bound.target.borrow().call,
                Callable::Native(entry) if std::ptr::fn_addr_eq(entry,indexed_get_missing_undefined as NativeFn)) =>
                bound.args.first()?.as_obj()?.clone(),
            _ => return None,
        };
        Some(IndexedRead {trap:Gc::downgrade(&trap),item:Gc::downgrade(&item)})
    });
    let entry = host_objects(i)
        .map
        .get_mut(&target_key)
        .expect("indexed native target has a host entry");
    assert!(
        entry.indexed_wrapper.is_none(),
        "native instance already has its indexed wrapper"
    );
    entry.indexed_wrapper = Some(Gc::downgrade(wrapper));
    entry.indexed_read = indexed_read;
}

/// Read the exact engine-created indexed wrapper without dispatching its
/// intrinsic bound Proxy trap. Values remain live; ordinary properties still
/// run the usual own/prototype getter walk with the original receiver.
pub(crate) fn indexed_wrapper_get(
    i:&mut Interp,wrapper:&Gc,target:&Value,handler:&Value,key:&str,receiver:&Value,
) -> Option<Result<Value,Abrupt>> {
    let target_object=target.as_obj()?;
    let entry=i.host_state.get::<HostObjects>()?.map.get(&(Gc::as_ptr(target_object) as usize))?;
    if !entry.indexed_wrapper.as_ref()?.upgrade().is_some_and(|registered|Gc::ptr_eq(&registered,wrapper)) {
        return None;
    }
    let read=entry.indexed_read.as_ref()?;
    let trap=read.trap.upgrade()?;
    let item=read.item.upgrade()?;
    let unchanged=handler.as_obj().is_some_and(|handler|handler.borrow().props.get("get")
        .filter(|property|!property.accessor()).and_then(|property|property.value().as_obj().cloned())
        .is_some_and(|current|Gc::ptr_eq(&current,&trap)));
    // Cross-realm trap dispatch owns its realm and invocation authorization.
    // Let that existing path run whenever it would switch the active realm.
    if !unchanged || i.multi_realm() && i.callee_realm_global(&trap).is_some() {return None;}
    if let Some(index)=crate::value::canonical_index(key) {
        match i.call(Value::Obj(item),target.clone(),&[Value::Num(index as f64)]) {
            Ok(Value::Undefined)=>{},
            result=>return Some(result),
        }
    }
    Some(i.get_member_recv(target,key,receiver.clone()))
}

#[derive(Default)]
struct IndexedClasses(FastMap<(RealmKey, TypeId), (Value, Option<NativeFn>)>);

/// Optional trusted indexed search. Length and fromIndex have already been evaluated by
/// Array.prototype.indexOf. An undefined result declines the optimization. Author Proxies
/// never acquire this capability, even when they wrap a native collection.
pub(crate) fn indexed_index_of(
    i: &mut Interp, receiver: &Value, target: &Value, from: usize, len: usize,
) -> Result<Option<Value>, Value> {
    let Some(object) = receiver.as_obj() else { return Ok(None); };
    if !i.proxies.contains_key(&(Gc::as_ptr(object) as usize)) { return Ok(None); }
    let Some(key) = host_entry_key(i, receiver) else { return Ok(None); };
    let Some(hook) = i.host_state.get::<HostObjects>().and_then(|objects| objects.map.get(&key))
        .and_then(|entry| entry.indexed_index_of) else { return Ok(None); };
    let result = hook(i, receiver.clone(), &[target.clone(), Value::Num(from as f64), Value::Num(len as f64)])?;
    Ok((!matches!(result, Value::Undefined)).then_some(result))
}

/// `hint(js(unforgeable))` getters of each class (its own and its base classes'), installed as
/// own non-configurable accessors on every instance (Web IDL `[LegacyUnforgeable]`). Keyed by
/// class and, for subclass lookups, by the class prototype.
#[derive(Default)]
struct Unforgeables {
    by_class: FastMap<(RealmKey, TypeId), Rc<[(Rc<str>, Value)]>>,
    by_proto: FastMap<usize, (WeakGc, Rc<[(Rc<str>, Value)]>)>,
}

/// Native class caches belong to their realm, rather than rooting every realm
/// ever installed. Count actual strong cache handles as heap edges and trace
/// their values when the owning global/prototype is reached by the collector.
pub(crate) fn count_host_class_cache_edges(i: &Interp, visit: &mut dyn FnMut(&Gc)) {
    let value = |value: &Value, visit: &mut dyn FnMut(&Gc)| {
        if let Value::Obj(object) = value { visit(object); }
    };
    if let Some(cache) = i.host_state.get::<ClassRegistry>() {
        for ((realm, _), (constructor, prototype)) in &cache.map {
            visit(&realm.0);
            value(constructor, visit);
            visit(prototype);
        }
        for (realm, _) in cache.pending.keys() { visit(&realm.0); }
    }
    if let Some(cache) = i.host_state.get::<IndexedClasses>() {
        for ((realm, _), (handler, _)) in &cache.0 {
            visit(&realm.0);
            value(handler, visit);
        }
    }
    if let Some(cache) = i.host_state.get::<Unforgeables>() {
        // A getter list is shared by two tables and may also be borrowed by
        // native code. Count its physical Value handles once, and leave an
        // externally retained list rooted until that actual holder releases it.
        let mut lists = FastMap::<usize, (&Rc<[(Rc<str>, Value)]>, usize)>::default();
        for ((realm, _), list) in &cache.by_class {
            visit(&realm.0);
            let entry = lists.entry(Rc::as_ptr(list) as *const () as usize).or_insert((list, 0));
            entry.1 += 1;
        }
        for (_, list) in cache.by_proto.values() {
            let entry = lists.entry(Rc::as_ptr(list) as *const () as usize).or_insert((list, 0));
            entry.1 += 1;
        }
        for (list, handles) in lists.values() {
            if Rc::strong_count(list) == *handles {
                for (_, getter) in list.iter() { value(getter, visit); }
            }
        }
    }
}

pub(crate) fn trace_host_class_cache(i: &Interp, pointer: usize, visit: &mut dyn FnMut(&Gc)) {
    let value = |value: &Value, visit: &mut dyn FnMut(&Gc)| {
        if let Value::Obj(object) = value { visit(object); }
    };
    let global = Gc::as_ptr(&i.global) as usize == pointer || i.realms.contains_key(&pointer);
    if let Some(cache) = i.host_state.get::<ClassRegistry>().filter(|_| global) {
        for ((realm, _), (constructor, prototype)) in &cache.map {
            if Gc::as_ptr(&realm.0) as usize == pointer {
                value(constructor, visit);
                visit(prototype);
            }
        }
    }
    if let Some(cache) = i.host_state.get::<IndexedClasses>().filter(|_| global) {
        for ((realm, _), (handler, _)) in &cache.0 {
            if Gc::as_ptr(&realm.0) as usize == pointer { value(handler, visit); }
        }
    }
    if let Some(cache) = i.host_state.get::<Unforgeables>() {
        if global {
            for ((realm, _), list) in &cache.by_class {
                if Gc::as_ptr(&realm.0) as usize == pointer {
                    for (_, getter) in list.iter() { value(getter, visit); }
                }
            }
        }
        if let Some((_, list)) = cache.by_proto.get(&pointer) {
            for (_, getter) in list.iter() { value(getter, visit); }
        }
    }
}

pub(crate) fn sweep_host_class_cache(i: &mut Interp, garbage: &[Gc]) {
    // The collector has already retained all garbage objects through its sweep,
    // so dropping these bookkeeping edges cannot invalidate a later heap walk.
    if garbage.is_empty() || (!i.host_state.has::<ClassRegistry>()
        && !i.host_state.has::<IndexedClasses>() && !i.host_state.has::<Unforgeables>()) { return; }
    // Keep scratch proportional to installed classes, not the whole dead heap.
    let mut cached = std::collections::HashSet::new();
    if let Some(cache) = i.host_state.get::<ClassRegistry>() {
        cached.extend(cache.map.keys().map(|(realm,_)|Gc::as_ptr(&realm.0) as usize));
        cached.extend(cache.pending.keys().map(|(realm,_)|Gc::as_ptr(&realm.0) as usize));
    }
    if let Some(cache) = i.host_state.get::<IndexedClasses>() {
        cached.extend(cache.0.keys().map(|(realm,_)|Gc::as_ptr(&realm.0) as usize));
    }
    if let Some(cache) = i.host_state.get::<Unforgeables>() {
        cached.extend(cache.by_class.keys().map(|(realm,_)|Gc::as_ptr(&realm.0) as usize));
        cached.extend(cache.by_proto.keys().copied());
    }
    let dead: std::collections::HashSet<usize> = garbage.iter().map(|object|Gc::as_ptr(object) as usize)
        .filter(|pointer|cached.contains(pointer)).collect();
    if let Some(cache) = i.host_state.get_mut::<ClassRegistry>() {
        cache.map.retain(|(realm, _), _| !dead.contains(&(Gc::as_ptr(&realm.0) as usize)));
        cache.pending.retain(|(realm, _), _| !dead.contains(&(Gc::as_ptr(&realm.0) as usize)));
    }
    if let Some(cache) = i.host_state.get_mut::<IndexedClasses>() {
        cache.0.retain(|(realm, _), _| !dead.contains(&(Gc::as_ptr(&realm.0) as usize)));
    }
    if let Some(cache) = i.host_state.get_mut::<Unforgeables>() {
        cache.by_class.retain(|(realm, _), _| !dead.contains(&(Gc::as_ptr(&realm.0) as usize)));
        cache.by_proto.retain(|pointer, _| !dead.contains(pointer));
    }
}

#[derive(Clone)]
struct NamedPropertyHooks {
    supported: Value,
    names: Value,
    getter: Value,
    override_builtins: bool,
}

fn indexed_handler(
    i: &mut Interp,
    item: NativeFn,
    length: NativeFn,
    setter: Option<NativeFn>,
    named: Option<NamedPropertyHooks>,
    undefined_means_missing: bool,
) -> Value {
    let handler = i.new_object();
    let item = Value::Obj(i.make_native("getitem", 1, item));
    let length = Value::Obj(i.make_native("length", 0, length));
    let setter = setter.map_or(Value::Undefined, |setter| {
        Value::Obj(i.make_native("setitem", 2, setter))
    });
    let (bound, traps) = if let Some(named) = named {
        (
            vec![
                item.clone(),
                length.clone(),
                named.supported,
                named.names,
                named.getter,
                setter,
                Value::Bool(named.override_builtins),
            ],
            vec![
                ("get", named_indexed_get as NativeFn),
                ("has", named_indexed_has),
                ("ownKeys", named_indexed_keys),
                ("getOwnPropertyDescriptor", named_indexed_descriptor),
                ("set", named_indexed_set),
                ("defineProperty", named_indexed_define),
                ("deleteProperty", named_indexed_delete),
                ("preventExtensions", indexed_prevent_extensions),
            ],
        )
    } else {
        (
            vec![item.clone(), length.clone(), setter],
            vec![
                ("get", if undefined_means_missing { indexed_get_missing_undefined as NativeFn } else { indexed_get as NativeFn }),
                ("has", indexed_has),
                ("ownKeys", indexed_keys),
                ("getOwnPropertyDescriptor", indexed_descriptor),
                ("set", indexed_set),
                ("defineProperty", indexed_define),
                ("deleteProperty", indexed_delete),
            ],
        )
    };
    for (name, entry) in traps {
        let function = crate::builtins::make_bound_len(i, entry, bound.clone(), 0.0);
        handler
            .borrow_mut()
            .props
            .insert(name, Property::builtin(function));
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
    let Some(objects) = i.host_state.get_mut::<HostObjects>() else {
        return;
    };
    for entry in objects.map.values_mut() {
        if entry
            .indexed_wrapper
            .as_ref()
            .is_some_and(|wrapper| wrapper.strong_count() == 0)
        {
            entry.indexed_wrapper = None;
        }
        if entry.identity || entry.callbacks_retained || entry.identity_owner.is_some() {
            let value = entry._weak.upgrade().map(Value::Obj);
            let expando = entry.identity_owner.is_none()
                && value.as_ref().is_some_and(|value| {
                    value
                        .as_obj()
                        .is_some_and(|obj| obj.borrow().props.iter().next().is_some())
                });
            entry.retained = if entry.callbacks_retained || expando || entry.identity_owner.is_some() {
                value
            } else {
                None
            };
        }
    }
}

/// Native owner pins are bookkeeping edges, like Interp::gc_pins. Another
/// interpreter's collection sees them as external holders; this interpreter
/// discounts its own pins so unreachable native cycles still collect.
pub(crate) fn count_host_identity_owner_pins(i: &Interp, visit: &mut dyn FnMut(&Value)) {
    let Some(objects) = i.host_state.get::<HostObjects>() else {
        return;
    };
    for entry in objects.map.values() {
        if entry.identity_owner.is_some()
            && (!entry.callbacks_retained || entry.native_values.is_some())
        {
            if let Some(value) = &entry.retained {
                visit(value);
            }
        }
        if let Some(trace) = entry.native_values {
            trace(entry, visit);
        }
    }
}

pub(crate) fn sweep_host_identity_owner_pin(i: &mut Interp, pointer: usize) {
    if let Some(entry) = i.host_state.get_mut::<HostObjects>()
        .and_then(|objects| objects.map.get_mut(&pointer))
    {
        if entry.identity_owner.is_some()
            && (!entry.callbacks_retained || entry.native_values.is_some())
        {
            entry.retained = None;
        }
    }
}

fn trace_identity_owner<T: NativeIdentityOwner>(
    entry: &HostEntry,
    epoch: u64,
    visit: &mut dyn FnMut(&Value),
) {
    with_identity_owner::<T>(entry, |owner| owner.trace_native_identities(epoch, visit));
}

fn trace_native_values<T: NativeIdentityOwner>(entry: &HostEntry, visit: &mut dyn FnMut(&Value)) {
    with_identity_owner::<T>(entry, |owner| owner.trace_native_values(visit));
}

fn with_identity_owner<T: NativeIdentityOwner>(entry: &HostEntry, visit: impl FnOnce(&T)) {
    let (pointer, guard) = (entry.view_ref)(&entry.data, TypeId::of::<T>())
        .expect("native identity owner projection is available during GC");
    // SAFETY: the native projection is immutable and its guard stays alive
    // through the trace, as in Ctx::with_instance.
    let owner = unsafe { &*pointer }
        .downcast_ref::<T>()
        .expect("matching native identity owner projection");
    visit(owner);
    drop(guard);
}

pub(crate) fn trace_host_identity_owner(
    i: &Interp,
    pointer: usize,
    visit: &mut dyn FnMut(&Value),
) {
    let Some(entry) = i
        .host_state
        .get::<HostObjects>()
        .and_then(|objects| objects.map.get(&pointer))
    else {
        return;
    };
    if let Some(trace) = entry.identity_owner {
        let epoch = crate::interpreter::GC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        trace(entry, epoch as u64, visit);
    }
    if let Some(trace) = entry.native_values {
        trace(entry, visit);
    }
}

pub(crate) fn retain_identity_receiver(i: &mut Interp, value: &Value) {
    let Some(key) = host_entry_key(i, value) else {
        return;
    };
    if let Some(entry) = i
        .host_state
        .get_mut::<HostObjects>()
        .and_then(|objects| objects.map.get_mut(&key))
    {
        if entry.identity {
            entry.retained = Some(value.clone());
        }
    }
}

fn index_key(value: &Value) -> Option<usize> {
    let Value::Str(key) = value else {
        return None;
    };
    let key = key.as_str();
    // Canonical decimal keys cannot contain signs or leading zeroes. Checking
    // the spelling directly avoids allocating a temporary decimal string on
    // every native indexed property access.
    if key.is_empty() || key.len() > 1 && key.starts_with('0')
        || !key.bytes().all(|byte| byte.is_ascii_digit()) { return None; }
    let index = key.parse::<usize>().ok()?;
    (index < u32::MAX as usize).then_some(index)
}

fn reflect(i: &mut Interp, name: &str, args: &[Value]) -> Result<Value, Value> {
    // Native indexed wrappers are engine-owned Proxies. Their behavior must use the actual
    // internal Reflect algorithms, not mutable `globalThis.Reflect` properties that author code
    // can replace between wrapper creation and trap invocation.
    let this = Value::Undefined;
    match name {
        "get" => crate::builtins::reflect::reflect_get(i, this, args),
        "has" => crate::builtins::reflect::reflect_has(i, this, args),
        "set" => crate::builtins::reflect::reflect_set(i, this, args),
        "defineProperty" => crate::builtins::reflect::reflect_define(i, this, args),
        "deleteProperty" => crate::builtins::reflect::reflect_delete(i, this, args),
        "ownKeys" => crate::builtins::reflect::reflect_own_keys(i, this, args),
        "getOwnPropertyDescriptor" => crate::builtins::reflect::reflect_gopd(i, this, args),
        _ => unreachable!("only native indexed wrapper traps call the Reflect helper"),
    }
}

fn named_prefix(named: bool) -> usize {
    if named { 7 } else { 3 }
}

fn indexed_setter_index(named: bool) -> usize {
    if named { 5 } else { 2 }
}

fn indexed_length_at(i: &mut Interp, args: &[Value], named: bool) -> Result<usize, Value> {
    let target_index = named_prefix(named);
    match i.invoke(args[1].clone(), args[target_index].clone(), &[])? {
        Value::Num(n) if n.is_finite() && n >= 0.0 => Ok(n as usize),
        _ => Err(i.make_error("TypeError", "indexed length must be a non-negative number")),
    }
}

fn invoke_named_supported(
    i: &mut Interp,
    args: &[Value],
    target: &Value,
    key: &Value,
) -> Result<bool, Value> {
    let value = i.invoke(args[2].clone(), target.clone(), std::slice::from_ref(key))?;
    Ok(i.to_boolean(&value))
}

fn named_property_visible(
    i: &mut Interp,
    args: &[Value],
    target: &Value,
    key: &Value,
) -> Result<bool, Value> {
    if !matches!(key, Value::Str(_)) || !invoke_named_supported(i, args, target, key)? {
        return Ok(false);
    }
    // Ordinary named properties are shadowed by own properties and the prototype chain.
    // [LegacyOverrideBuiltIns] changes that rule only for inherited properties: an own
    // expando still takes precedence over the named property.
    let present = if matches!(args.get(6), Some(Value::Bool(true))) {
        !matches!(
            reflect(
                i,
                "getOwnPropertyDescriptor",
                &[target.clone(), key.clone()]
            )?,
            Value::Undefined
        )
    } else {
        let inherited = reflect(i, "has", &[target.clone(), key.clone()])?;
        i.to_boolean(&inherited)
    };
    Ok(!present)
}

fn has_inherited_setter(i: &mut Interp, target: &Value, key: &Value) -> Result<bool, Value> {
    let mut current = i.prototype_of(target);
    while matches!(current, Value::Obj(_)) {
        let descriptor = reflect(
            i,
            "getOwnPropertyDescriptor",
            &[current.clone(), key.clone()],
        )?;
        if !matches!(descriptor, Value::Undefined) {
            let setter = i.get_member(&descriptor, "set").map_err(abrupt_value)?;
            return Ok(!matches!(setter, Value::Undefined));
        }
        current = i.prototype_of(&current);
    }
    Ok(false)
}

fn named_property_value(
    i: &mut Interp,
    args: &[Value],
    target: &Value,
    key: &Value,
) -> Result<Value, Value> {
    i.invoke(args[4].clone(), target.clone(), std::slice::from_ref(key))
}

fn is_indexed_wrapper_receiver(i: &Interp, target: &Value, receiver: &Value) -> bool {
    let (Some(target), Some(receiver)) = (target.as_obj(), receiver.as_obj()) else {
        return false;
    };
    let Some(entry) = i
        .host_state
        .get::<HostObjects>()
        .and_then(|objects| objects.map.get(&(Gc::as_ptr(target) as usize)))
    else {
        return false;
    };
    entry
        .indexed_wrapper
        .as_ref()
        .and_then(WeakGc::upgrade)
        .is_some_and(|wrapper| Gc::ptr_eq(&wrapper, receiver))
}

fn named_property_names(
    i: &mut Interp,
    args: &[Value],
    target: &Value,
) -> Result<Vec<Value>, Value> {
    let names = i.invoke(args[3].clone(), target.clone(), &[])?;
    let count = i.get_member(&names, "length").map_err(abrupt_value)?;
    let Value::Num(count) = count else {
        return Err(i.make_error("TypeError", "named property names must be an array"));
    };
    if !count.is_finite() || count < 0.0 {
        return Err(i.make_error("TypeError", "named property names length is invalid"));
    }
    let count = count as usize;
    let mut values = Vec::new();
    values.try_reserve(count).map_err(|_| {
        i.make_error(
            "RangeError",
            "named property key list exceeds available memory",
        )
    })?;
    for index in 0..count {
        values.push(
            i.get_member(&names, &index.to_string())
                .map_err(abrupt_value)?,
        );
    }
    Ok(values)
}

fn property_descriptor(i: &mut Interp, value: Value, enumerable: bool, writable: bool) -> Value {
    let descriptor = i.new_object();
    for (name, value) in [
        ("value", value),
        ("writable", Value::Bool(writable)),
        ("enumerable", Value::Bool(enumerable)),
        ("configurable", Value::Bool(true)),
    ] {
        descriptor
            .borrow_mut()
            .props
            .insert(name, Property::builtin(value));
    }
    Value::Obj(descriptor)
}

fn indexed_get_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    if let Some(index) = args.get(prefix + 1).and_then(index_key) {
        if index < indexed_length_at(i, args, named)? {
            i.invoke(
                args[0].clone(),
                args[prefix].clone(),
                &[Value::Num(index as f64)],
            )
        } else {
            // Only supported indices are own platform properties; an unsupported index
            // still participates in the ordinary prototype walk.
            reflect(i, "get", &args[prefix..])
        }
    } else {
        let target = &args[prefix];
        let key = &args[prefix + 1];
        if named && named_property_visible(i, args, target, key)? {
            named_property_value(i, args, target, key)
        } else {
            reflect(i, "get", &args[prefix..])
        }
    }
}

// An opt-in class contract: getitem returns undefined exactly for unsupported
// indices. Query the value once instead of dispatching a second length native.
// Missing indices still perform the ordinary own/prototype walk, and all other
// traps keep using the native supported-index length.
fn indexed_get_missing_undefined(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    let prefix = named_prefix(false);
    if let Some(index) = args.get(prefix + 1).and_then(index_key) {
        let value = i.invoke(args[0].clone(), args[prefix].clone(), &[Value::Num(index as f64)])?;
        if !matches!(value, Value::Undefined) { return Ok(value); }
    }
    reflect(i, "get", &args[prefix..])
}

fn indexed_get(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_get_inner(i, args, false)
}

fn named_indexed_get(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_get_inner(i, args, true)
}

fn indexed_has_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    if let Some(index) = args.get(prefix + 1).and_then(index_key) {
        if index < indexed_length_at(i, args, named)? { return Ok(Value::Bool(true)); }
        reflect(i, "has", &args[prefix..])
    } else {
        let target = &args[prefix];
        let key = &args[prefix + 1];
        let present = reflect(i, "has", &[target.clone(), key.clone()])?;
        Ok(Value::Bool(
            i.to_boolean(&present) || (named && invoke_named_supported(i, args, target, key)?),
        ))
    }
}

fn indexed_has(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_has_inner(i, args, false)
}

fn named_indexed_has(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_has_inner(i, args, true)
}

fn indexed_set_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    let Some(index) = args.get(prefix + 1).and_then(index_key) else {
        if named {
            let (target, key) = (args[prefix].clone(), args[prefix + 1].clone());
            // Legacy platform [[Set]] ignores named properties when finding the own
            // descriptor, then the ordinary algorithm creates a data property on the
            // receiver, which [[DefineOwnProperty]] rejects for a supported name without a
            // named setter. An inherited setter still runs.
            if matches!(key, Value::Str(_)) && invoke_named_supported(i, args, &target, &key)? {
                let override_builtins = matches!(args.get(6), Some(Value::Bool(true)));
                let own = reflect(
                    i,
                    "getOwnPropertyDescriptor",
                    &[target.clone(), key.clone()],
                )?;
                if (override_builtins || matches!(own, Value::Undefined))
                    && !has_inherited_setter(i, &target, &key)?
                {
                    return Ok(Value::Bool(false));
                }
            }
            let receiver = args.get(prefix + 3).cloned().unwrap_or(Value::Undefined);
            if is_indexed_wrapper_receiver(i, &args[prefix], &receiver) {
                // Legacy platform [[Set]] ignores the virtual named property
                // and performs ordinary setting on the platform object. Using
                // the raw target as receiver avoids re-entering this proxy's
                // defineProperty trap when an assignment creates an expando.
                let set_args = [
                    args[prefix].clone(),
                    args[prefix + 1].clone(),
                    args[prefix + 2].clone(),
                    args[prefix].clone(),
                ];
                return reflect(i, "set", &set_args);
            }
        }
        return reflect(i, "set", &args[prefix..]);
    };
    let setter = args
        .get(indexed_setter_index(named))
        .cloned()
        .unwrap_or(Value::Undefined);
    if matches!(setter, Value::Undefined) {
        return Ok(Value::Bool(false));
    }
    let value = args.get(prefix + 2).cloned().unwrap_or(Value::Undefined);
    i.invoke(
        setter,
        args[prefix].clone(),
        &[Value::Num(index as f64), value],
    )?;
    Ok(Value::Bool(true))
}

fn indexed_set(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_set_inner(i, args, false)
}

fn named_indexed_set(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_set_inner(i, args, true)
}

fn indexed_define_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    let key = &args[prefix + 1];
    if let Some(index) = index_key(key) {
        let setter = args
            .get(indexed_setter_index(named))
            .cloned()
            .unwrap_or(Value::Undefined);
        if matches!(setter, Value::Undefined) {
            return Ok(Value::Bool(false));
        }
        let descriptor = args[prefix + 2].clone();
        let has_value = reflect(i, "has", &[descriptor.clone(), Value::str("value")])?;
        if !i.to_boolean(&has_value) {
            return Ok(Value::Bool(false));
        }
        let value = reflect(
            i,
            "get",
            &[descriptor.clone(), Value::str("value"), descriptor],
        )?;
        i.invoke(
            setter,
            args[prefix].clone(),
            &[Value::Num(index as f64), value],
        )?;
        Ok(Value::Bool(true))
    } else {
        // WebIDL legacy platform [[DefineOwnProperty]]: with no named setter a supported
        // name rejects the definition unless an own property exists and the interface lacks
        // [LegacyOverrideBuiltIns]. Prototype-chain shadowing does not matter here.
        if named
            && matches!(key, Value::Str(_))
            && invoke_named_supported(i, args, &args[prefix], key)?
        {
            let override_builtins = matches!(args.get(6), Some(Value::Bool(true)));
            let own = reflect(
                i,
                "getOwnPropertyDescriptor",
                &[args[prefix].clone(), key.clone()],
            )?;
            if override_builtins || matches!(own, Value::Undefined) {
                return Ok(Value::Bool(false));
            }
        }
        reflect(i, "defineProperty", &args[prefix..])
    }
}

fn indexed_define(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_define_inner(i, args, false)
}

fn named_indexed_define(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_define_inner(i, args, true)
}

fn indexed_delete_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    if let Some(index) = args.get(prefix + 1).and_then(index_key) {
        Ok(Value::Bool(index >= indexed_length_at(i, args, named)?))
    } else {
        let target = &args[prefix];
        let key = &args[prefix + 1];
        if named && named_property_visible(i, args, target, key)? {
            return Ok(Value::Bool(false));
        }
        reflect(i, "deleteProperty", &args[prefix..])
    }
}

fn indexed_delete(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_delete_inner(i, args, false)
}

fn named_indexed_delete(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_delete_inner(i, args, true)
}

fn indexed_keys_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    let length = indexed_length_at(i, args, named)?;
    let own = reflect(i, "ownKeys", &args[prefix..])?;
    let count = i.get_member(&own, "length").map_err(abrupt_value)?;
    let mut keys: Vec<Value> = (0..length)
        .map(|index| Value::str(index.to_string()))
        .collect();
    let mut named_seen = HashSet::<LStr>::new();
    if named {
        let names = named_property_names(i, args, &args[prefix])?;
        named_seen.try_reserve(names.len()).map_err(|_| {
            i.make_error(
                "RangeError",
                "named property key list exceeds available memory",
            )
        })?;
        for key in names {
            // Canonical array-index strings are always handled by indexed lookup, even if there
            // is no indexed item at that position. Excluding them also keeps ownKeys duplicate-
            // free when an attribute happens to have a numeric qualified name.
            if index_key(&key).is_some() || !named_property_visible(i, args, &args[prefix], &key)? {
                continue;
            }
            if let Value::Str(name) = &key {
                if named_seen.insert(name.clone()) {
                    keys.push(key);
                }
            }
        }
    }
    if let Value::Num(count) = count {
        for index in 0..count as usize {
            let key = i
                .get_member(&own, &index.to_string())
                .map_err(abrupt_value)?;
            if named {
                if let Value::Str(name) = &key {
                    // Indexed properties already lead the result, and named properties were
                    // appended above. A transient set avoids a quadratic scan as the actual
                    // target's own string keys are merged while preserving their source order.
                    if index_key(&key).is_some_and(|key_index| key_index < length)
                        || !named_seen.insert(name.clone())
                    {
                        continue;
                    }
                }
            } else if matches!(&key, Value::Str(name) if keys.iter().any(|existing| matches!(existing, Value::Str(known) if known == name)))
            {
                continue;
            }
            keys.push(key);
        }
    }
    Ok(JsHost::from_list(i, keys))
}

fn indexed_keys(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_keys_inner(i, args, false)
}

fn named_indexed_keys(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_keys_inner(i, args, true)
}

fn indexed_descriptor_inner(i: &mut Interp, args: &[Value], named: bool) -> Result<Value, Value> {
    let prefix = named_prefix(named);
    let target = &args[prefix];
    let key = &args[prefix + 1];
    if let Some(index) = index_key(key) {
        if index >= indexed_length_at(i, args, named)? {
            return Ok(Value::Undefined);
        }
        let value = i.invoke(args[0].clone(), target.clone(), &[Value::Num(index as f64)])?;
        let writable = !matches!(
            args.get(indexed_setter_index(named)),
            None | Some(Value::Undefined)
        );
        Ok(property_descriptor(i, value, true, writable))
    } else {
        if named && named_property_visible(i, args, target, key)? {
            let value = named_property_value(i, args, target, key)?;
            return Ok(property_descriptor(i, value, false, false));
        }
        reflect(i, "getOwnPropertyDescriptor", &args[prefix..])
    }
}

fn indexed_descriptor(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_descriptor_inner(i, args, false)
}

fn named_indexed_descriptor(i: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    indexed_descriptor_inner(i, args, true)
}

fn indexed_prevent_extensions(_: &mut Interp, _: Value, _: &[Value]) -> Result<Value, Value> {
    // Legacy platform objects remain extensible so dynamic indices/names cannot violate Proxy
    // invariants after the engine has reported that a target is non-extensible.
    Ok(Value::Bool(false))
}

fn attach_instance<T: Class>(i: &mut Interp, obj: &Gc, value: T) {
    if T::DESC.hint("js", "error").is_some() {
        let stack = i.capture_trace(None);
        obj.borrow_mut().set_exotic(Exotic::Error, Some(stack));
    }
    if let Some(accessors) = i
        .host_state
        .get::<Unforgeables>()
        .and_then(|u| u.by_class.get(&(active_realm_key(i), TypeId::of::<T>())))
        .cloned()
    {
        let mut object = obj.borrow_mut();
        for (name, getter) in accessors.iter() {
            object.props.insert(
                &**name,
                Property::accessor_prop(Some(getter.clone()), None, true, false),
            );
        }
    }
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
            indexed_wrapper: None,
            indexed_read: None,
            indexed_index_of: None,
            retained: None,
            identity: false,
            callbacks_retained: false,
            identity_owner: None,
            native_values: None,
            view_ref: view_ref::<T>,
            view_mut: view_mut::<T>,
        },
    );
}

fn sweep(i: &mut Interp) -> usize {
    let t = host_objects(i);
    for entry in t.map.values_mut() {
        if entry
            .indexed_wrapper
            .as_ref()
            .is_some_and(|wrapper| wrapper.strong_count() == 0)
        {
            entry.indexed_wrapper = None;
        }
    }
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
    let key = (active_realm_key(i), TypeId::of::<T>());
    i.host_state
        .get::<ClassRegistry>()
        .and_then(|r| r.map.get(&key))
        .cloned()
}

/// `T`'s registration, building it first when `T` belongs to a lazily installed module.
fn resolved_class<T: Class>(i: &mut Interp) -> Option<(Value, Gc)> {
    if let Some(entry) = registered_class::<T>(i) {
        return Some(entry);
    }
    let key = (active_realm_key(i), class_name::<T>());
    let make = i
        .host_state
        .get::<ClassRegistry>()
        .and_then(|r| r.pending.get(&key))
        .copied()?;
    make(i).ok()?;
    registered_class::<T>(i)
}

#[cold]
fn unregistered<T: Class>(i: &mut Interp) -> Value {
    let msg = format!(
        "class {} is not registered with this engine (define its module or class first)",
        class_name::<T>()
    );
    i.make_error("TypeError", msg)
}

/// This realm's constructor + prototype for `T`, created on first use.
fn class_entry<T: Methods<JsHost>>(i: &mut Interp) -> (Value, Gc) {
    if let Some(e) = registered_class::<T>(i) {
        return e;
    }
    let mut members = Vec::new();
    T::members(&mut members);
    members.retain(|m| m.desc.exposed_to("js"));
    // Web IDL has enumerable named operations and attributes. Ordinary native
    // classes retain JavaScript class/builtin descriptor conventions.
    let webidl = T::DESC.hint("js", "webidl").is_some();
    let named_properties = T::DESC.hint("js", "named_properties").is_some();
    let override_builtins = T::DESC.hint("js", "override_builtins").is_some();
    if override_builtins && !named_properties {
        panic!("override_builtins requires named native properties");
    }
    let realm_key = active_realm_key(i);
    let indexed_members = (
        members
            .iter()
            .find(|m| m.desc.role == Role::Proto("getitem")),
        members.iter().find(|m| m.desc.role == Role::Proto("len")),
        members
            .iter()
            .find(|m| m.desc.role == Role::Proto("setitem")),
    );
    if named_properties && (indexed_members.0.is_none() || indexed_members.1.is_none()) {
        panic!("named native properties require indexed getter and length hooks");
    }
    let named_hooks = if named_properties {
        let hook = |key| {
            members
                .iter()
                .find(|member| member.desc.hint("js", key).is_some())
                .unwrap_or_else(|| panic!("named native properties require {key} hook"))
        };
        let supported = hook("named_supported");
        let names = hook("named_names");
        let getter = hook("named_getter");
        for member in [supported, names, getter] {
            register_op(i, member);
        }
        Some(NamedPropertyHooks {
            supported: Value::Obj(i.make_native(
                supported.desc.name,
                supported.desc.min_pos as usize,
                supported.entry,
            )),
            names: Value::Obj(i.make_native(
                names.desc.name,
                names.desc.min_pos as usize,
                names.entry,
            )),
            getter: Value::Obj(i.make_native(
                getter.desc.name,
                getter.desc.min_pos as usize,
                getter.entry,
            )),
            override_builtins,
        })
    } else {
        None
    };
    if let (Some(item), Some(length), setter) = indexed_members {
        if !i.host_state.has::<IndexedClasses>() {
            i.host_state.put(IndexedClasses::default());
        }
        let handler = indexed_handler(
            i,
            item.entry,
            length.entry,
            setter.map(|setter| setter.entry),
            named_hooks,
            T::DESC.hint("js", "indexed_missing_undefined").is_some(),
        );
        let index_of = members.iter().find(|member| member.desc.role == Role::Proto("indexof"));
        if let Some(member) = index_of { register_op(i, member); }
        i.host_state
            .get_mut::<IndexedClasses>()
            .unwrap()
            .0
            .insert((realm_key.clone(), TypeId::of::<T>()), (handler, index_of.map(|member| member.entry)));
    }
    let name = class_name::<T>();
    let base = T::base_class(i).unwrap_or_else(|_| panic!("native base class registration"));
    let base_proto = base
        .as_ref()
        .and_then(|base| base.as_obj())
        .and_then(|base| {
            base.borrow()
                .props
                .get("prototype")
                .and_then(|p| p.value().as_obj().cloned())
        });
    let iterator_class = T::DESC.hint("js", "iterator").is_some();
    let error_class = T::DESC.hint("js", "error").is_some();
    let parent_proto = if iterator_class {
        i.extra_protos.get("%IteratorPrototype%").cloned()
    } else if error_class && base_proto.is_none() {
        i.error_protos.get("Error").cloned()
    } else {
        None
    };
    let base = match base {
        None if error_class => parent_proto
            .as_ref()
            .and_then(|proto| proto.borrow().props.get("constructor").map(|p| p.value())),
        base => base,
    };
    let proto = Object::new(Some(
        parent_proto
            .or(base_proto.clone())
            .unwrap_or_else(|| i.object_proto.clone()),
    ));
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
        if let Some(base) = base.and_then(|base| base.as_obj().cloned()) {
            c.proto = Some(base);
        }
        c.props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
    }
    proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(ctor.clone()), true, false, true),
    );
    if iterator_class {
        proto.borrow_mut().props.remove("constructor");
    }
    crate::builtins::set_to_string_tag(i, &proto, name);
    // Accessors: pair getters and setters by name.
    let mut accessors: Vec<(Cow<'static, str>, Option<Value>, Option<Value>)> = Vec::new();
    let mut unforgeable: Vec<(Rc<str>, Value)> = base_proto
        .as_ref()
        .and_then(|base| {
            i.host_state
                .get::<Unforgeables>()
                .and_then(|u| u.by_proto.get(&(Gc::as_ptr(base) as usize)))
                .filter(|(weak, _)| weak.upgrade().is_some_and(|live| Gc::ptr_eq(&live, base)))
                .map(|(_, list)| list.to_vec())
        })
        .unwrap_or_default();
    for m in &members {
        let d = m.desc;
        let js = js_name(d);
        let named_hook = ["named_supported", "named_names", "named_getter"]
            .iter()
            .any(|hint| d.hint("js", hint).is_some());
        let installed = !named_hook
            && match d.role {
                Role::Constructor | Role::Function => false,
                Role::Proto(p) => matches!(p, "iter" | "next" | "str" | "len"),
                _ => true,
            };
        if !installed {
            continue;
        }
        register_op(i, m);
        match d.role {
            Role::Method if d.hint("js", "symbol_for").is_some() => {
                let description = d.hint("js", "symbol_for").unwrap_or_default();
                let key = symbol_for_key(i, description);
                let f = i.make_native(&format!("[{description}]"), d.min_pos as usize, m.entry);
                proto
                    .borrow_mut()
                    .props
                    .insert(key, Property::builtin(Value::Obj(f)));
            }
            Role::Method | Role::Proto("next" | "str") => {
                let length = d
                    .hint("js", "length")
                    .and_then(|length| length.parse().ok())
                    .unwrap_or(d.min_pos as usize);
                i.def_method(&proto, &js, length, m.entry);
                if webidl {
                    if let Some(property) = proto.borrow_mut().props.get_mut(&*js) {
                        property.set_enumerable(true);
                    }
                }
                if d.hint("js", "also_iterator").is_some() {
                    let method = proto.borrow().props.get(&*js).map(|property| property.value());
                    if let (Some(method), Some(key)) =
                        (method, crate::builtins::well_known_key(i, "iterator"))
                    {
                        proto
                            .borrow_mut()
                            .props
                            .insert(key, Property::builtin(method));
                    }
                }
            }
            Role::Static => {
                i.def_method(&ctor, &js, d.min_pos as usize, m.entry);
                if webidl {
                    if let Some(property) = ctor.borrow_mut().props.get_mut(&*js) {
                        property.set_enumerable(true);
                    }
                }
            }
            Role::Proto("iter") => {
                let f = i.make_native("[Symbol.iterator]", 0, m.entry);
                if let Some(key) = crate::builtins::well_known_key(i, "iterator") {
                    proto
                        .borrow_mut()
                        .props
                        .insert(key, Property::builtin(Value::Obj(f)));
                }
            }
            Role::Getter if d.hint("js", "symbol_for").is_some() => {
                let description = d.hint("js", "symbol_for").unwrap_or_default();
                let key = symbol_for_key(i, description);
                let f = Value::Obj(i.make_native(&format!("get [{description}]"), 0, m.entry));
                proto
                    .borrow_mut()
                    .props
                    .insert(key, Property::accessor_prop(Some(f), None, false, true));
            }
            Role::Getter | Role::Proto("len") => {
                let f = Value::Obj(i.make_native(&format!("get {js}"), 0, m.entry));
                if d.hint("js", "unforgeable").is_some() {
                    unforgeable.push((Rc::<str>::from(&*js), f.clone()));
                }
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
            .insert(&*name, Property::accessor_prop(get, set, webidl, true));
    }
    let mut constants = Vec::new();
    T::constants(&mut constants);
    for constant in constants {
        let value = (constant.value)(i)
            .unwrap_or_else(|_| panic!("class constant {} does not convert", constant.name));
        for target in [&ctor, &proto] {
            target
                .borrow_mut()
                .props
                .insert(constant.name, Property::data(value.clone(), false, true, false));
        }
    }
    if !unforgeable.is_empty() {
        let list: Rc<[(Rc<str>, Value)]> = unforgeable.into();
        if !i.host_state.has::<Unforgeables>() {
            i.host_state.put(Unforgeables::default());
        }
        let registry = i.host_state.get_mut::<Unforgeables>().unwrap();
        registry
            .by_proto
            .retain(|_, (weak, _)| weak.upgrade().is_some());
        registry.by_proto.insert(
            Gc::as_ptr(&proto) as usize,
            (Gc::downgrade(&proto), list.clone()),
        );
        registry
            .by_class
            .insert((realm_key.clone(), TypeId::of::<T>()), list);
    }
    let entry = (Value::Obj(ctor), proto);
    if !i.host_state.has::<ClassRegistry>() {
        i.host_state.put(ClassRegistry::default());
    }
    i.host_state
        .get_mut::<ClassRegistry>()
        .unwrap()
        .map
        .insert((realm_key, TypeId::of::<T>()), entry.clone());
    entry
}

/// The property key of the registry symbol `Symbol.for(description)`.
fn symbol_for_key(i: &mut Interp, description: &str) -> String {
    let Value::Sym(data) = i.symbol_for(description) else {
        unreachable!("symbol_for must return a symbol")
    };
    Interp::sym_key(&data)
}

// ---- lazy globals ----------------------------------------------------------------------------

/// What a lazily published global becomes on first access.
enum LazyItem {
    Function(FnItem<JsHost>),
    Class(Make<JsHost>),
    /// A module constant; it may instantiate the module's classes, so they are registered first.
    Constant(Make<JsHost>, Rc<[Make<JsHost>]>),
    /// One of several globals that one initializer defines together (see [`LazyGroup`]).
    Group(Rc<LazyGroup>),
}

/// Runs a group's initializer with the interpreter and the global object the group lives on.
pub type LazyGroupInit = Rc<dyn Fn(&mut Interp, &Value) -> Result<(), Value>>;

/// Globals defined as a side effect of one initializer (typically evaluating JS glue). Each name
/// is a lazy accessor; the first read or reflection through any of them removes every accessor
/// the group still owns and then runs the initializer once, which defines the real properties.
/// A name a script replaced before that keeps the script's value.
struct LazyGroup {
    init: RefCell<Option<LazyGroupInit>>,
    /// Weak: each slot's accessor owns its slot, and the slot owns the group.
    slots: RefCell<Vec<std::rc::Weak<LazyGlobal>>>,
}

impl LazyGroup {
    fn run(&self, i: &mut Interp, global: &Gc) -> Result<(), Value> {
        let Some(init) = self.init.borrow_mut().take() else {
            return Ok(());
        };
        for slot in self.slots.take().iter().filter_map(std::rc::Weak::upgrade) {
            if slot.owns(global) {
                global.borrow_mut().props.remove(&slot.name);
            }
        }
        init(i, &Value::Obj(global.clone()))
    }
}

/// One lazily published global: an accessor on its realm's global object that replaces itself
/// with the final data property the first time it is read (or written, which stores the assigned
/// value instead).
struct LazyGlobal {
    /// The global object it was published on. Weak: the object owns the accessor.
    global: WeakGc,
    name: String,
    item: LazyItem,
    enumerable: bool,
    /// The accessor's getter, so the slot can tell whether the property is still its own.
    getter: RefCell<Option<std::rc::Weak<crate::value::NativeClosure>>>,
}

impl LazyGlobal {
    fn owns(&self, global: &Gc) -> bool {
        let Some(mine) = self.getter.borrow().as_ref().and_then(std::rc::Weak::upgrade) else {
            return false;
        };
        global.borrow().props.get(&self.name).is_some_and(|p| {
            p.accessor()
                && p.getter()
                    .and_then(Value::as_obj)
                    .is_some_and(|getter| match &getter.borrow().call {
                        Callable::NativeData(data) => Rc::ptr_eq(&data.func, &mine),
                        _ => false,
                    })
        })
    }

    /// Replace the accessor with a writable data property that keeps the accessor's current
    /// enumerable and configurable attributes (a script may have changed them meanwhile).
    fn publish(&self, global: &Gc, value: Value) {
        let mut global = global.borrow_mut();
        let (enumerable, configurable) = global
            .props
            .get(&self.name)
            .map_or((self.enumerable, true), |p| (p.enumerable(), p.configurable()));
        global.props.remove(&self.name);
        global.props.insert(
            self.name.as_str(),
            Property::data(value, true, enumerable, configurable),
        );
    }

    fn get(&self, i: &mut Interp) -> Result<Value, Value> {
        let Some(global) = self.global.upgrade() else {
            return Ok(Value::Undefined);
        };
        if !self.owns(&global) {
            return i
                .get_member(&Value::Obj(global), &self.name)
                .map_err(abrupt_value);
        }
        if let LazyItem::Group(group) = &self.item {
            group.run(i, &global)?;
            return i
                .get_member(&Value::Obj(global), &self.name)
                .map_err(abrupt_value);
        }
        let value = match &self.item {
            LazyItem::Group(_) => unreachable!("handled above"),
            LazyItem::Function(f) => i.bound_function(f),
            LazyItem::Class(make) => make(i)?,
            LazyItem::Constant(make, classes) => {
                for class in classes.iter() {
                    class(i)?;
                }
                make(i)?
            }
        };
        self.publish(&global, value.clone());
        Ok(value)
    }

    fn set(&self, i: &mut Interp, value: Value) -> Result<Value, Value> {
        let Some(global) = self.global.upgrade() else {
            return Ok(Value::Undefined);
        };
        if self.owns(&global) {
            self.publish(&global, value);
        } else {
            i.set_member(&Value::Obj(global), &self.name, value)
                .map_err(abrupt_value)?;
        }
        Ok(Value::Undefined)
    }
}

/// Publish one lazy accessor named `name` on `global` unless the realm already defines it.
fn install_lazy_slot(
    ctx: &mut Interp,
    global: &Gc,
    name: String,
    item: LazyItem,
    enumerable: bool,
) -> Option<Rc<LazyGlobal>> {
    if global.borrow().props.contains(&name) {
        return None;
    }
    let slot = Rc::new(LazyGlobal {
        global: Gc::downgrade(global),
        name,
        item,
        enumerable,
        getter: RefCell::new(None),
    });
    let get: Rc<crate::value::NativeClosure> = {
        let slot = slot.clone();
        Rc::new(move |i: &mut Interp, _: Value, _: &[Value]| slot.get(i))
    };
    *slot.getter.borrow_mut() = Some(Rc::downgrade(&get));
    let set: Rc<crate::value::NativeClosure> = {
        let slot = slot.clone();
        Rc::new(move |i: &mut Interp, _: Value, args: &[Value]| {
            slot.set(i, args.first().cloned().unwrap_or(Value::Undefined))
        })
    };
    let materialize = slot.clone();
    ctx.register_lazy_global(
        global,
        &slot.name,
        Rc::new(move |i: &mut Interp| materialize.get(i).map(|_| ())),
    );
    let getter = Value::Obj(ctx.make_native_closure(&format!("get {}", slot.name), 0, get));
    let setter = Value::Obj(ctx.make_native_closure(&format!("set {}", slot.name), 1, set));
    global.borrow_mut().props.insert(
        slot.name.as_str(),
        Property::accessor_prop(Some(getter), Some(setter), enumerable, true),
    );
    Some(slot)
}

/// Publish everything `items` declares as lazy accessors on the global object. A global the
/// realm already has is left alone: earlier providers stay canonical.
fn install_items_lazy(ctx: &mut Interp, items: ModuleItems<JsHost>) -> Result<(), Value> {
    let global = ctx.global.clone();
    let mut published: Vec<(String, LazyItem, bool)> = Vec::new();
    for f in items.functions.iter().filter(|f| f.desc.exposed_to("js")) {
        let enumerable = f.desc.hint("js", "webidl").is_some();
        published.push((js_name(f.desc).into_owned(), LazyItem::Function(*f), enumerable));
    }
    for c in items.classes.iter().filter(|c| c.desc.exposed_to("js")) {
        published.push((c.desc.name_for("js").to_string(), LazyItem::Class(c.object), false));
    }
    let classes: Rc<[Make<JsHost>]> = items
        .classes
        .iter()
        .filter(|c| c.desc.exposed_to("js"))
        .map(|c| c.object)
        .collect();
    if !ctx.host_state.has::<ClassRegistry>() {
        ctx.host_state.put(ClassRegistry::default());
    }
    let realm = active_realm_key(ctx);
    // `skip(js)` classes have no global but are still value types (e.g. iterators), so they
    // must be buildable on demand too.
    for c in items.classes.iter() {
        ctx.host_state
            .get_mut::<ClassRegistry>()
            .unwrap()
            .pending
            .insert((realm.clone(), c.desc.name_for("js")), c.object);
    }
    for k in &items.constants {
        let item = LazyItem::Constant(k.value, classes.clone());
        published.push((k.name.to_string(), item, k.enumerable));
    }
    for (name, item, enumerable) in published {
        install_lazy_slot(ctx, &global, name, item, enumerable);
    }
    if let Some(init) = items.init {
        init(ctx, &Value::Obj(global))?;
    }
    Ok(())
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
    /// Run `on_ok(value)` or `on_err(reason)` once `value` (any value, a thenable is adopted) has
    /// settled, like `await value`, through the engine's own promise machinery. The reaction runs
    /// in a microtask even for a plain value.
    pub fn then_value(&mut self, value: Value, on_ok: Value, on_err: Value) {
        let promise = match self.promise_resolve_checked(value) {
            Ok(promise) => promise,
            Err(reason) => {
                let promise = self.new_promise();
                self.reject_promise(&promise, reason);
                promise
            }
        };
        self.promise_then(&promise, on_ok, on_err);
    }

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

    /// Publish everything `#[module]` `M` declares as lazy globals: each function, class and
    /// constant is an accessor on the global object that builds the real value on first access
    /// and replaces itself with a data property (writable and configurable; enumerable only for
    /// an `#[op]` with `hint(js(webidl))`). Assigning to the accessor first stores the assigned
    /// value instead. A name the realm already defines is skipped; `#[init]` runs immediately,
    /// with the global object.
    pub fn install_module_lazy<M: Module<JsHost>>(&mut self) -> Result<(), Value> {
        install_items_lazy(self, ModuleItems::of::<M>())
    }

    /// Publish `names` as non-enumerable lazy globals of the current realm that `init` defines
    /// together. The first access to any of them (a read, or reflection such as
    /// `Object.getOwnPropertyDescriptor(globalThis, name)` / `Object.keys`) removes the group's
    /// remaining accessors and runs `init(interp, global)` exactly once; `init` then defines the
    /// real properties, so they read as ordinary data properties afterwards. Assigning to a name
    /// first replaces just that accessor with the assigned value, and `init` must leave such an
    /// override alone. A name the realm already defines is skipped (earlier providers stay
    /// canonical); `in` does not run `init`.
    pub fn install_lazy_global_group(&mut self, names: &[&str], init: LazyGroupInit) {
        let global = self.global.clone();
        let group = Rc::new(LazyGroup {
            init: RefCell::new(Some(init)),
            slots: RefCell::new(Vec::new()),
        });
        for name in names {
            let item = LazyItem::Group(group.clone());
            if let Some(slot) = install_lazy_slot(self, &global, (*name).to_string(), item, false) {
                group.slots.borrow_mut().push(Rc::downgrade(&slot));
            }
        }
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

    /// Install typed global-interface attributes on the actual global object.
    /// Members tagged `hint(js(global))` are own attributes, rather than
    /// prototype attributes. Derive their names/descriptors from the same
    /// declarations used by class installation; no second registration list.
    pub fn install_global_attributes<T: Methods<JsHost>>(&mut self) -> Result<(), Value> {
        let (_, prototype) = class_entry::<T>(self);
        let prototype = Value::Obj(prototype);
        let global = self.global_object();
        let mut members = Vec::new();
        T::members(&mut members);
        for member in members {
            if member.desc.role != Role::Getter
                || !member.desc.exposed_to("js")
                || member.desc.hint("js", "global").is_none()
            {
                continue;
            }
            let name = js_name(member.desc);
            let key = Value::str(&*name);
            let descriptor = self.reflect_get_own_property_descriptor(&prototype, &key)?;
            // A cached class whose attribute was already moved must preserve
            // author replacement, redefinition, and deletion on reinstall.
            if matches!(descriptor, Value::Undefined) { continue; }
            if !self.reflect_define_property(&global, &key, &descriptor)?
            {
                return Err(self.make_error("TypeError", "global attribute could not be installed"));
            }
            if !self.delete_member(&prototype, &name)? {
                return Err(self.make_error("TypeError", "global attribute prototype could not be removed"));
            }
        }
        Ok(())
    }

    /// Wrap a Rust value as a new JS instance of its class.
    pub fn new_instance<T: Methods<JsHost>>(&mut self, value: T) -> Value {
        let (_, proto) = class_entry::<T>(self);
        new_instance(self, value, proto)
    }

    /// Give an existing host object a native class and its prototype.
    pub fn attach_instance<T: Methods<JsHost>>(
        &mut self,
        object: &Value,
        value: T,
    ) -> OpResult<()> {
        let Some(object) = object.as_obj() else {
            return Err(OpError::new(
                "TypeError",
                "native instance requires an object",
            ));
        };
        let (_, prototype) = class_entry::<T>(self);
        object.borrow_mut().proto = Some(prototype);
        attach_instance(self, object, value);
        Ok(())
    }

    /// Give an existing object the native state of class `T` without changing its prototype (a
    /// realm global or a host object that already has its prototype chain becomes an
    /// `EventTarget`). Fails when the object already has native state.
    pub fn attach_native_data<T: Methods<JsHost>>(
        &mut self,
        object: &Value,
        value: T,
    ) -> OpResult<()> {
        let Some(object) = object.as_obj() else {
            return Err(OpError::new(
                "TypeError",
                "native instance requires an object",
            ));
        };
        class_entry::<T>(self);
        if host_objects(self)
            .map
            .contains_key(&(Gc::as_ptr(object) as usize))
        {
            return Err(OpError::new(
                "TypeError",
                "object already has native instance state",
            ));
        }
        attach_instance(self, object, value);
        Ok(())
    }

    /// The Rust value behind a class instance (shared handle; borrow it with the `RefCell`
    /// API). `None` when `v` is not a `T` instance.
    pub fn instance_data<T: Class>(&self, v: &Value) -> Option<Rc<RefCell<T>>> {
        host_data(self, v)?.downcast::<RefCell<T>>().ok()
    }

    /// Whether `v` is an instance of any native (`lumen_bind`) class, a subclass created by
    /// script included.
    pub fn is_native_instance(&self, v: &Value) -> bool {
        host_entry_key(self, v).is_some()
    }

    /// Read a native instance or one of its embedded base classes.
    pub fn with_instance<T: Class, R>(
        &mut self,
        value: &Value,
        read: impl FnOnce(&T) -> R,
    ) -> OpResult<R> {
        let key = host_entry_key_checked(self, value)
            .map_err(OpError::thrown)?
            .ok_or_else(|| OpError::new("TypeError", "value is not a native instance"))?;
        let entry = self
            .host_state
            .get::<HostObjects>()
            .and_then(|objects| objects.map.get(&key))
            .ok_or_else(|| OpError::new("TypeError", "value is not a native instance"))?;
        let (pointer, guard) = (entry.view_ref)(&entry.data, TypeId::of::<T>()).map_err(|_| {
            OpError::new(
                "TypeError",
                "native instance has the wrong class or is already in use",
            )
        })?;
        let data = unsafe { &*pointer }
            .downcast_ref::<T>()
            .expect("matching native projection");
        let result = read(data);
        drop(guard);
        Ok(result)
    }

    /// Mutably access a native instance or one of its embedded base classes.
    pub fn with_instance_mut<T: Class, R>(
        &mut self,
        value: &Value,
        update: impl FnOnce(&mut T) -> R,
    ) -> OpResult<R> {
        let key = host_entry_key_checked(self, value)
            .map_err(OpError::thrown)?
            .ok_or_else(|| OpError::new("TypeError", "value is not a native instance"))?;
        let entry = self
            .host_state
            .get::<HostObjects>()
            .and_then(|objects| objects.map.get(&key))
            .ok_or_else(|| OpError::new("TypeError", "value is not a native instance"))?;
        let (pointer, guard) = (entry.view_mut)(&entry.data, TypeId::of::<T>()).map_err(|_| {
            OpError::new(
                "TypeError",
                "native instance has the wrong class or is already in use",
            )
        })?;
        let data = unsafe { &mut *pointer }
            .downcast_mut::<T>()
            .expect("matching mutable native projection");
        let result = update(data);
        drop(guard);
        Ok(result)
    }

    /// Keep a host wrapper alive while its native owner retains callbacks.
    pub fn retain_instance(&mut self, v: &Value, retained: bool) {
        let Some(key) = host_entry_key(self, v) else {
            return;
        };
        if let Some(entry) = host_objects(self).map.get_mut(&key) {
            entry.callbacks_retained = retained;
            entry.retained = (retained || entry.identity_owner.is_some()).then(|| v.clone());
        }
    }

    /// Replace blanket expando retention with tracing from reachable native
    /// owners. Embedded base classes are accepted by the existing projection.
    pub fn set_native_identity_owner<T: NativeIdentityOwner>(
        &mut self,
        value: &Value,
    ) -> OpResult<()> {
        self.with_instance::<T, _>(value, |_| ())?;
        let key = host_entry_key(self, value)
            .expect("validated native identity owner has an entry");
        let entry = host_objects(self)
            .map
            .get_mut(&key)
            .expect("validated native identity owner entry exists");
        entry.identity_owner = Some(trace_identity_owner::<T>);
        entry.native_values = T::TRACES_NATIVE_VALUES.then_some(trace_native_values::<T>);
        entry.retained = Some(value.clone());
        Ok(())
    }

    /// [`Self::set_native_identity_owner`] unless the instance already has an owner (a subclass
    /// that traces more state, such as a DOM node, keeps its own).
    pub fn ensure_native_identity_owner<T: NativeIdentityOwner>(
        &mut self,
        value: &Value,
    ) -> OpResult<()> {
        let owned = host_entry_key(self, value)
            .and_then(|key| host_objects(self).map.get(&key))
            .is_some_and(|entry| entry.identity_owner.is_some());
        if owned {
            return Ok(());
        }
        self.set_native_identity_owner::<T>(value)
    }

    /// One lazy native wrapper per identity key in this realm.
    pub fn cached_instance<T: Methods<JsHost>, K: Eq + std::hash::Hash + Clone + 'static>(
        &mut self,
        key: K,
        make: impl FnOnce() -> T,
    ) -> Value {
        if let Some(value) = self
            .host_state
            .get::<IdentityCache<T, K>>()
            .and_then(|cache| cache.map.get(&key))
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = self.new_instance(make());
        if let Some(object) = value.as_obj() {
            let target = self
                .proxies
                .get(&(Gc::as_ptr(object) as usize))
                .and_then(|(target, _)| target.as_obj())
                .unwrap_or(object);
            let ptr = Gc::as_ptr(target) as usize;
            if let Some(entry) = host_objects(self).map.get_mut(&ptr) {
                entry.identity = true;
            }
            object.borrow().ic_plain.set(false);
            self.inline_ic_safe.set(false);
        }
        if !self.host_state.has::<IdentityCache<T, K>>() {
            self.host_state.put(IdentityCache::<T, K> {
                map: std::collections::HashMap::new(),
                sweep_at: 256,
                _class: std::marker::PhantomData,
            });
        }
        let weak = self.weak_value(&value).expect("native identity wrapper");
        let cache = self.host_state.get_mut::<IdentityCache<T, K>>().unwrap();
        if cache.map.len() >= cache.sweep_at {
            cache.map.retain(|_, value| value.upgrade().is_some());
            cache.sweep_at = cache.map.len().saturating_mul(2).max(256);
        }
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

    /// Read the internal time value from a genuine Date object without
    /// invoking author-mutable Date prototype methods. Proxies do not carry
    /// the Date time slot and therefore return `None`.
    pub fn date_value(&self, value: &Value) -> Option<f64> {
        let Value::Obj(object) = value else {
            return None;
        };
        object.borrow().date_value()
    }

    /// Construct a Date instance in this realm from an already converted
    /// millisecond time value, using the engine's existing Date prototype and
    /// internal slot rather than looking up the mutable global constructor.
    pub fn new_date_value(&mut self, milliseconds: f64) -> Value {
        let prototype = self.extra_protos.get("Date").cloned();
        let object = Object::new(prototype);
        object
            .borrow_mut()
            .set_exotic(
                Exotic::Date,
                Some(Value::Num(crate::builtins::time_clip(milliseconds))),
            );
        Value::Obj(object)
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

    /// A new plain object whose own data properties are `entries` (enumerable, writable,
    /// configurable), defined directly so no setter on `Object.prototype` can observe them.
    pub fn plain_object(&mut self, entries: &[(&str, Value)]) -> Value {
        let object = self.new_object();
        {
            let mut object = object.borrow_mut();
            for (name, value) in entries {
                object.props.insert(*name, Property::plain(value.clone()));
            }
        }
        Value::Obj(object)
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

    /// Define everything `#[module]` `M` declares as lazy globals (see
    /// [`Interp::install_module_lazy`]).
    pub fn define_lazy_globals<M: Module<JsHost>>(&mut self) -> Result<(), Value> {
        self.interp.install_module_lazy::<M>()
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
    use crate::embed::Ctx;

    macro_rules! callback_identity_owner {
        ($class:ident, $name:literal, $traced:expr) => {
            #[lumen_bind::class(name = $name)]
            struct $class {
                callback: Option<JsFunction>,
            }
            #[lumen_bind::methods]
            impl $class {
                #[constructor]
                fn new() -> Self { Self { callback: None } }
                #[getter]
                fn callback(&self) -> Option<JsFunction> { self.callback.clone() }
                #[setter]
                fn set_callback(&mut self, callback: Option<JsFunction>) {
                    self.callback = callback;
                }
            }
            impl NativeIdentityOwner for $class {
                const TRACES_NATIVE_VALUES: bool = $traced;
                fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
                fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
                    if let Some(callback) = &self.callback { visit(callback.value()); }
                }
            }
        };
    }
    callback_identity_owner!(TracedCallbackOwner, "TracedCallbackOwner", true);
    callback_identity_owner!(OpaqueCallbackOwner, "OpaqueCallbackOwner", false);

    #[test]
    fn native_class_caches_collect_retired_realms_and_preserve_retained_constructors() {
        let mut engine = crate::Engine::new();
        let realm = engine.ctx().create_host_realm();
        let key = realm.key();
        let global = realm.global();
        let weak = engine.ctx().weak_value(&global).unwrap();
        let constructor = engine.ctx().with_host_realm(&realm, |ctx|ctx.class_constructor::<TracedCallbackOwner>()).unwrap();
        engine.ctx().dispose_host_realm(&realm).unwrap();
        drop(global);
        drop(realm);
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(),"a retained native constructor preserves its origin realm");
        assert!(engine.ctx().realms.contains_key(&key));
        drop(constructor);
        engine.collect_garbage();
        assert!(weak.upgrade().is_none(),"class installation caches do not root retired realms");
        assert!(!engine.ctx().realms.contains_key(&key));
    }

    #[test]
    fn native_callback_owner_edges_preserve_reachable_callbacks_and_collect_cycles() {
        let mut engine = crate::Engine::new();
        engine.define_class::<TracedCallbackOwner>();
        native_eval(&mut engine, "(()=>{const owner=new TracedCallbackOwner();owner.callback=()=>owner;globalThis.callbackOwner=owner})()").ok().expect("traced owner setup succeeds");
        let owner = native_eval(&mut engine, "callbackOwner").ok().expect("traced owner exists");
        let weak = engine.ctx().weak_value(&owner).unwrap();
        engine.ctx().set_native_identity_owner::<TracedCallbackOwner>(&owner).unwrap();
        engine.ctx().retain_instance(&owner, true);
        drop(owner);
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(), "reachable native owner survives");
        assert!(matches!(native_eval(&mut engine, "callbackOwner.callback()===callbackOwner"), Ok(Value::Bool(true))));
        native_eval(&mut engine, "callbackOwner=null").ok().expect("traced owner root clears");
        engine.collect_garbage();
        assert!(weak.upgrade().is_none(), "unreachable native callback cycle collects");
    }

    #[test]
    fn native_callback_owner_without_opt_in_preserves_opaque_callback_pin() {
        let mut engine = crate::Engine::new();
        engine.define_class::<OpaqueCallbackOwner>();
        native_eval(&mut engine, "(()=>{const owner=new OpaqueCallbackOwner();owner.callback=()=>owner;globalThis.opaqueOwner=owner})()").ok().expect("opaque owner setup succeeds");
        let owner = native_eval(&mut engine, "opaqueOwner").ok().expect("opaque owner exists");
        let weak = engine.ctx().weak_value(&owner).unwrap();
        engine.ctx().set_native_identity_owner::<OpaqueCallbackOwner>(&owner).unwrap();
        engine.ctx().retain_instance(&owner, true);
        drop(owner);
        native_eval(&mut engine, "opaqueOwner=null").ok().expect("opaque owner root clears");
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(), "opaque callback owner keeps existing retention policy");
    }

    macro_rules! forwarded_native_class {
        ($class:ident, $name:literal, $attribute:literal) => {
            #[lumen_bind::class(name = $name, hint(js(webidl)))]
            struct $class;
            #[lumen_bind::methods]
            impl $class {
                #[constructor]
                fn new() -> Self {
                    Self
                }
                #[getter(name = $attribute)]
                fn stored_value(&self) -> i32 {
                    17
                }
            }
        };
    }
    forwarded_native_class!(MacroForwardedClass, "ForwardedClass", "storedValue");

    #[test]
    fn macro_forwarded_native_class_and_attribute_names_are_reflected() {
        let mut engine = crate::Engine::new();
        engine.define_class::<MacroForwardedClass>();
        let result = native_eval(
            &mut engine,
            "(()=>{const value=new ForwardedClass();const descriptor=Object.getOwnPropertyDescriptor(ForwardedClass.prototype,'storedValue');return ForwardedClass.name==='ForwardedClass'&&value.storedValue===17&&descriptor.enumerable&&descriptor.configurable})()",
        );
        assert!(matches!(result, Ok(Value::Bool(true))));
    }

    #[lumen_bind::class(name = "WebIdlDescriptor", hint(js(webidl)))]
    struct WebIdlDescriptor {
        value: i32,
    }

    #[lumen_bind::methods]
    impl WebIdlDescriptor {
        #[constructor]
        fn new(value: i32) -> Self {
            Self { value }
        }

        #[getter]
        fn value(&self) -> i32 {
            self.value
        }

        #[setter]
        fn set_value(&mut self, value: i32) {
            self.value = value;
        }

        #[method]
        fn operation(&self) -> i32 {
            self.value
        }

        #[method]
        fn static_operation() -> i32 {
            42
        }
    }

    #[lumen_bind::class(name = "OrdinaryDescriptor")]
    struct OrdinaryDescriptor;

    #[lumen_bind::methods]
    impl OrdinaryDescriptor {
        #[constructor]
        fn new() -> Self {
            Self
        }

        #[getter]
        fn value(&self) -> i32 {
            1
        }

        #[method]
        fn operation(&self) -> i32 {
            1
        }
    }

    #[lumen_bind::class(name = "NativeReceiverBase", hint(js(webidl)))]
    struct NativeReceiverBase {
        marker: i32,
    }

    #[lumen_bind::methods]
    impl NativeReceiverBase {
        #[constructor]
        fn new(marker: i32) -> Self {
            Self { marker }
        }

        #[getter]
        fn marker(&self) -> i32 {
            self.marker
        }
    }

    #[lumen_bind::class(
        name = "NativeReceiverDerived",
        extends = NativeReceiverBase,
        hint(js(webidl))
    )]
    struct NativeReceiverDerived {
        base: NativeReceiverBase,
    }

    #[lumen_bind::methods]
    impl NativeReceiverDerived {
        #[constructor]
        fn new(marker: i32) -> Self {
            Self {
                base: NativeReceiverBase { marker },
            }
        }
    }

    #[lumen_bind::class(name = "NativeIndexedWrapper", hint(js(webidl)))]
    struct NativeIndexedWrapper {
        values: Vec<i32>,
    }

    #[lumen_bind::methods]
    impl NativeIndexedWrapper {
        #[constructor]
        fn new() -> Self {
            Self {
                values: vec![5, 8, 13],
            }
        }

        #[proto(getitem)]
        fn item(&self, index: usize) -> i32 {
            self.values.get(index).copied().unwrap_or(-1)
        }

        #[proto(len)]
        fn length(&self) -> usize {
            self.values.len()
        }

        #[method]
        fn sum(&self) -> i32 {
            self.values.iter().sum()
        }
    }

    #[lumen_bind::class(name = "NativeSingleProbeIndexed", hint(js(webidl, indexed_missing_undefined)))]
    struct NativeSingleProbeIndexed {
        values: Vec<i32>,
        lengths: std::cell::Cell<usize>,
    }
    #[lumen_bind::methods]
    impl NativeSingleProbeIndexed {
        #[constructor]
        fn new() -> Self { Self { values: vec![5,8,13], lengths: std::cell::Cell::new(0) } }
        #[proto(getitem)]
        fn item(&self, index: usize) -> Value {
            self.values.get(index).map_or(Value::Undefined, |value| Value::Num(*value as f64))
        }
        #[proto(len)]
        fn length(&self) -> usize {
            self.lengths.set(self.lengths.get()+1);
            self.values.len()
        }
        #[getter]
        fn length_checks(&self) -> usize { self.lengths.get() }
        #[method]
        fn push(&mut self, value: i32) { self.values.push(value); }
    }

    #[lumen_bind::class(name = "NativeMutableIndexedWrapper", hint(js(webidl)))]
    struct NativeMutableIndexedWrapper {
        values: Vec<i32>,
    }

    #[lumen_bind::methods]
    impl NativeMutableIndexedWrapper {
        #[constructor]
        fn new() -> Self {
            Self { values: vec![3] }
        }

        #[proto(getitem)]
        fn item(&self, index: usize) -> i32 {
            self.values.get(index).copied().unwrap_or(-1)
        }

        #[proto(len)]
        fn length(&self) -> usize {
            self.values.len()
        }

        #[proto(setitem)]
        fn set_item(
            ctx: &mut Ctx,
            this: lumen_bind::This<Value>,
            index: usize,
            value: i32,
        ) -> OpResult<()> {
            let length = ctx.with_instance::<Self, _>(&this.0, |indexed| indexed.values.len())?;
            if index > length {
                return Err(OpError::range_error("indexed assignment is not contiguous"));
            }
            ctx.with_instance_mut::<Self, _>(&this.0, |indexed| {
                if index == length {
                    indexed.values.push(value);
                } else {
                    indexed.values[index] = value;
                }
            })?;
            Ok(())
        }
    }

    #[lumen_bind::class(
        name = "NativeOverrideNamedWrapper",
        hint(js(webidl, named_properties, override_builtins))
    )]
    struct NativeOverrideNamedWrapper {
        enabled: Cell<bool>,
    }

    #[lumen_bind::methods]
    impl NativeOverrideNamedWrapper {
        #[constructor]
        fn new() -> Self {
            Self {
                enabled: Cell::new(false),
            }
        }

        #[proto(getitem)]
        fn item(&self, index: usize) -> i32 {
            if index == 0 { 23 } else { -1 }
        }

        #[proto(len)]
        fn length(&self) -> usize {
            1
        }

        #[method]
        fn collision(&self) -> i32 {
            41
        }

        #[method]
        fn enable_named(&self) {
            self.enabled.set(true);
        }

        #[method(hint(js(named_supported)))]
        fn named_supported(&self, name: &str) -> bool {
            self.enabled.get() && name == "collision"
        }

        #[method(hint(js(named_names)))]
        fn named_names(&self) -> Vec<String> {
            if self.enabled.get() {
                vec!["collision".to_owned()]
            } else {
                Vec::new()
            }
        }

        #[method(hint(js(named_getter)))]
        fn named_getter(&self, name: &str) -> String {
            format!("named:{name}")
        }
    }

    fn native_eval(engine: &mut crate::Engine, source: &str) -> Result<Value, Value> {
        engine
            .eval_value(source)
            .expect("valid native regression source")
    }

    fn published_deferred(engine: &mut crate::Engine, name: &str) -> Deferred {
        let deferred = Deferred::new(engine.ctx());
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, name, deferred.promise()).ok().expect("publish native promise");
        deferred
    }

    #[test]
    fn registered_deferred_is_published_before_init_and_keeps_original_settlement() {
        let mut engine = crate::Engine::new();
        let init = native_eval(&mut engine, r#"
            globalThis.initCount = 0;
            (promise => {
                initCount++;
                if (promise !== published) throw Error('host identity not published');
                globalThis.hookPromise = promise;
            })
        "#).ok().expect("init hook");
        engine.ctx().set_promise_hooks(Some([init, Value::Undefined, Value::Undefined, Value::Undefined]));
        let deferred = Deferred::new_registered(engine.ctx(), |ctx, deferred| {
            let global = ctx.global_object();
            ctx.member_set(&global, "published", deferred.promise()).ok().expect("publish identity");
            deferred
        });
        engine.ctx().set_promise_hooks(None);
        deferred.resolve(engine.ctx(), 17);
        let result = native_eval(&mut engine,
            "initCount === 1 && hookPromise === published").ok().expect("hook identity");
        assert!(matches!(result, Value::Bool(true)));
        native_eval(&mut engine, "published.then(value => { globalThis.result = value; })")
            .ok().expect("observe settlement");
        engine.ctx().drain_microtasks();
        assert!(matches!(native_eval(&mut engine, "result").ok(), Some(Value::Num(17.0))));
    }

    fn synchronously_rejected_subclass(engine: &mut crate::Engine) -> (Value, Value, Value) {
        let init = native_eval(engine,
            "globalThis.initializedPromises = []; (p => initializedPromises.push(p))")
            .ok().expect("capture native constructor promise");
        engine.ctx().set_promise_hooks(Some([init, Value::Undefined, Value::Undefined, Value::Undefined]));
        let promise = native_eval(engine, r#"
            globalThis.reason = {marker: 19};
            class SynchronouslyRejectedPromise extends Promise {}
            globalThis.rejectedSubclass = new SynchronouslyRejectedPromise((resolve, reject) => reject(reason));
            rejectedSubclass
        "#).ok().expect("synchronously rejected subclass");
        let alias = native_eval(engine, "initializedPromises[0]").ok().expect("native source alias");
        let reason = native_eval(engine, "reason").ok().expect("subclass rejection reason");
        assert_ne!(alias.object_identity(), promise.object_identity(), "the native promise is grafted onto the subclass instance");
        (promise, alias, reason)
    }

    #[test]
    fn native_rejection_reason_preserves_exact_result_without_author_operations() {
        let mut engine = crate::Engine::new();
        let pending = published_deferred(&mut engine, "pendingReason");
        assert!(engine.ctx().promise_rejection_reason(&pending.promise()).is_none());
        let fulfilled = published_deferred(&mut engine, "fulfilledReason");
        let fulfilled_promise = fulfilled.promise();
        fulfilled.resolve(engine.ctx(), Value::Num(3.0));
        assert!(engine.ctx().promise_rejection_reason(&fulfilled_promise).is_none());
        let reason = native_eval(&mut engine, "globalThis.exactReason = {marker: 23}; exactReason")
            .ok().expect("native reason");
        let rejected = published_deferred(&mut engine, "rejectedReason");
        let promise = rejected.promise();
        rejected.reject_handled(engine.ctx(), reason.clone());
        native_eval(&mut engine, r#"
            Object.defineProperty(rejectedReason, 'then', {get() {throw new Error('author then');}});
            Object.defineProperty(rejectedReason, 'reason', {get() {throw new Error('author reason');}});
        "#).ok().expect("poison author properties");
        let actual = engine.ctx().promise_rejection_reason(&promise).expect("rejected result");
        assert_eq!(actual.object_identity(), reason.object_identity());
        assert_eq!(engine.ctx().promise_is_handled(&promise), Some(true));
        assert!(engine.ctx().promise_rejection_reason(&Value::Num(3.0)).is_none());
        assert!(engine.take_unhandled_rejections_full().is_empty());
    }

    #[test]
    fn native_rejection_reason_follows_subclass_alias_and_preserves_undefined() {
        let mut engine = crate::Engine::new();
        let (promise, alias, reason) = synchronously_rejected_subclass(&mut engine);
        for identity in [promise, alias] {
            let actual = engine.ctx().promise_rejection_reason(&identity).expect("subclass result");
            assert_eq!(actual.object_identity(), reason.object_identity());
        }
        let rejected = Deferred::new(engine.ctx());
        let promise = rejected.promise();
        rejected.reject_handled(engine.ctx(), Value::Undefined);
        assert!(matches!(engine.ctx().promise_rejection_reason(&promise), Some(Value::Undefined)));
    }

    #[test]
    fn deferred_rejection_sync_subclass_reports_public_identity_in_original_order() {
        let mut engine = crate::Engine::new();
        let owner = engine.ctx().current_host_realm();
        let (promise, _, reason) = synchronously_rejected_subclass(&mut engine);
        let later = published_deferred(&mut engine, "laterRejected");
        let later_promise = later.promise();
        later.reject(engine.ctx(), reason.clone());
        engine.run_microtasks();
        let reported = engine.take_unhandled_rejections_with_realm();
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].1.object_identity(), promise.object_identity(), "the reported promise must be the public subclass instance");
        assert_eq!(reported[1].1.object_identity(), later_promise.object_identity(), "grafting preserves original rejection order");
        assert!(reported.iter().all(|(realm, _, rejected)| realm.same_realm(&owner) && rejected.object_identity() == reason.object_identity()));
    }

    #[test]
    fn deferred_rejection_sync_subclass_public_slot_mark_clears_original_record() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let (promise, alias, _) = synchronously_rejected_subclass(&mut engine);
        assert!(engine.ctx().mark_promise_handled(&promise));
        assert_eq!(engine.ctx().promise_is_handled(&alias), Some(true));
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty(), "marking the graft target removes the original pending rejection");
        assert!(engine.take_late_handled_rejections().is_empty());
    }

    #[test]
    fn deferred_rejection_sync_subclass_source_alias_mark_clears_original_record() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let (promise, alias, _) = synchronously_rejected_subclass(&mut engine);
        assert!(engine.ctx().mark_promise_handled(&alias));
        assert_eq!(engine.ctx().promise_is_handled(&promise), Some(true));
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty(), "marking the captured native source alias removes its grafted pending rejection");
        assert!(engine.take_late_handled_rejections().is_empty());
    }

    #[test]
    fn deferred_rejection_sync_subclass_early_catch_removes_pending_without_late_event() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let (_, alias, _) = synchronously_rejected_subclass(&mut engine);
        native_eval(&mut engine, "rejectedSubclass.catch(e => { globalThis.earlySubclassReason = e; });")
            .ok().expect("early subclass catch");
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty(), "an early author catch removes the grafted pending record");
        assert!(engine.take_late_handled_rejections().is_empty(), "the rejection was never reported");
        assert_eq!(engine.ctx().promise_is_handled(&alias), Some(true));
        assert!(matches!(native_eval(&mut engine, "earlySubclassReason === reason").ok(), Some(Value::Bool(true))));
    }

    #[test]
    fn deferred_rejection_sync_subclass_late_catch_reports_public_identity_once() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let (promise, alias, reason) = synchronously_rejected_subclass(&mut engine);
        engine.run_microtasks();
        let reported = engine.take_unhandled_rejections_full();
        assert_eq!(reported.len(), 1);
        native_eval(&mut engine, "rejectedSubclass.catch(e => { globalThis.lateSubclassReason = e; });")
            .ok().expect("late subclass catch");
        engine.run_microtasks();
        let late = engine.take_late_handled_rejections();
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].object_identity(), promise.object_identity());
        assert_eq!(reported[0].0.object_identity(), promise.object_identity());
        assert_eq!(reported[0].1.object_identity(), reason.object_identity());
        assert_eq!(engine.ctx().promise_is_handled(&alias), Some(true));
        native_eval(&mut engine, "rejectedSubclass.catch(() => {});").ok().expect("repeat catch");
        engine.run_microtasks();
        assert!(engine.take_late_handled_rejections().is_empty());
        assert!(matches!(native_eval(&mut engine, "lateSubclassReason === reason").ok(), Some(Value::Bool(true))));
    }

    #[test]
    fn deferred_rejection_preserves_ordinary_early_and_late_tracking() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let reason = native_eval(&mut engine, "globalThis.reason = {marker: 7}").ok().expect("reason");
        let ordinary = published_deferred(&mut engine, "ordinary");
        let ordinary_promise = ordinary.promise();
        ordinary.reject(engine.ctx(), reason.clone());
        engine.run_microtasks();
        let reported = engine.take_unhandled_rejections_full();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].0.object_identity(), ordinary_promise.object_identity());
        assert_eq!(reported[0].1.object_identity(), reason.object_identity());
        assert!(engine.take_unhandled_rejections_full().is_empty());
        native_eval(&mut engine, "ordinary.catch(e => { globalThis.lateReason = e; });").ok().expect("late catch");
        engine.run_microtasks();
        let late = engine.take_late_handled_rejections();
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].object_identity(), ordinary_promise.object_identity());
        native_eval(&mut engine, "ordinary.catch(() => {});").ok().expect("second catch");
        engine.run_microtasks();
        assert!(engine.take_late_handled_rejections().is_empty());

        let early = published_deferred(&mut engine, "early");
        native_eval(&mut engine, "early.catch(e => { globalThis.earlyReason = e; });").ok().expect("early catch");
        assert_eq!(engine.ctx().promise_is_handled(&early.promise()), Some(true));
        early.reject(engine.ctx(), reason.clone());
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty());
        assert!(engine.take_late_handled_rejections().is_empty());

        let before_report = published_deferred(&mut engine, "beforeReport");
        before_report.reject(engine.ctx(), reason);
        native_eval(&mut engine, "beforeReport.catch(e => { globalThis.beforeReportReason = e; });").ok().expect("catch before report");
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty());
        assert!(engine.take_late_handled_rejections().is_empty());
        assert!(matches!(native_eval(&mut engine,
            "lateReason === reason && earlyReason === reason && beforeReportReason === reason").ok(), Some(Value::Bool(true))));
    }

    #[test]
    fn deferred_rejection_handled_is_intrinsic_and_preserves_exact_reason() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let init = native_eval(&mut engine,
            "globalThis.promiseInits = 0; (() => { promiseInits++; })").ok().expect("promise hook");
        engine.ctx().set_promise_hooks(Some([init, Value::Undefined, Value::Undefined, Value::Undefined]));
        let deferred = published_deferred(&mut engine, "internallyHandled");
        let promise = deferred.promise();
        let reason = OpError::new("AbortError", "animation was canceled").to_value(engine.ctx());
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, "abortReason", reason.clone()).ok().expect("publish reason");
        native_eval(&mut engine, r#"
            globalThis.savedCatch = Promise.prototype.catch;
            Promise.prototype.then = function() { throw new Error('author then invoked'); };
            Promise.prototype.catch = function() { throw new Error('author catch invoked'); };
        "#).ok().expect("poison author operations");
        assert!(engine.ctx().microtasks.is_empty());
        deferred.reject_handled(engine.ctx(), reason);
        assert_eq!(engine.ctx().promise_is_handled(&promise), Some(true));
        assert!(engine.ctx().microtasks.is_empty(), "slot marking queues no synthetic reaction");
        if let Value::Obj(object) = &promise {
            let object = object.borrow();
            let crate::value::Callable::Promise(slot) = &object.call else { panic!("native promise slot") };
            assert!(slot.first.is_none() && slot.rest.is_none(), "slot marking adds no reaction");
        }
        assert!(engine.take_unhandled_rejections_full().is_empty());
        assert!(engine.take_late_handled_rejections().is_empty());
        assert!(matches!(native_eval(&mut engine, "promiseInits === 1").ok(), Some(Value::Bool(true))),
            "slot marking allocates no dependent promise");
        // Restore only then: the captured standard catch invokes the current then.
        let then = engine.ctx().promise_intr().expect("promise intrinsics").then.clone();
        let global = engine.ctx().global_object();
        let constructor = engine.ctx().get_member(&global, "Promise").ok().expect("Promise");
        let prototype = engine.ctx().get_member(&constructor, "prototype").ok().expect("Promise prototype");
        engine.ctx().member_set(&prototype, "then", Value::Obj(then)).ok().expect("restore then");
        native_eval(&mut engine, "savedCatch.call(internallyHandled, e => { globalThis.observedAbort = e; });").ok().expect("author observes rejection");
        engine.run_microtasks();
        assert!(matches!(native_eval(&mut engine,
            "observedAbort === abortReason && observedAbort.name === 'AbortError' && observedAbort.message === 'animation was canceled'").ok(), Some(Value::Bool(true))));
        assert!(engine.take_unhandled_rejections_full().is_empty());
        assert!(engine.take_late_handled_rejections().is_empty());
    }

    #[test]
    fn deferred_rejection_handled_preserves_independent_derived_failures() {
        let mut engine = crate::Engine::new();
        let reason = native_eval(&mut engine, "globalThis.reason = {marker: 11}").ok().expect("reason");
        let deferred = published_deferred(&mut engine, "source");
        let source = deferred.promise();
        assert!(engine.ctx().mark_promise_handled(&source));
        assert_eq!(engine.ctx().promise_is_handled(&source), Some(true));
        native_eval(&mut engine, "globalThis.derived = source.then();").ok().expect("independent derived promise");
        deferred.reject_handled(engine.ctx(), reason.clone());
        engine.run_microtasks();
        native_eval(&mut engine, "globalThis.unrelated = Promise.reject(reason);").ok().expect("unrelated author rejection");
        let reported = engine.take_unhandled_rejections_full();
        assert_eq!(reported.len(), 2, "internal handling must not silence dependent or unrelated failures");
        let global = engine.ctx().global_object();
        let derived = engine.ctx().get_member(&global, "derived").ok().expect("derived promise");
        let unrelated = engine.ctx().get_member(&global, "unrelated").ok().expect("unrelated promise");
        assert_eq!(reported[0].0.object_identity(), derived.object_identity());
        assert_eq!(reported[1].0.object_identity(), unrelated.object_identity());
        assert!(reported.iter().all(|(_, rejected)| rejected.object_identity() == reason.object_identity()));
    }

    #[test]
    fn deferred_rejection_direct_slot_mark_does_not_invent_late_handling_events() {
        let mut engine = crate::Engine::new();
        engine.track_late_handled_rejections();
        let reason = native_eval(&mut engine, "globalThis.reason = {marker: 17}").ok().expect("reason");
        let pending = published_deferred(&mut engine, "markedPending");
        assert!(engine.ctx().mark_promise_handled(&pending.promise()));
        pending.reject(engine.ctx(), reason.clone());
        engine.run_microtasks();
        assert!(engine.take_unhandled_rejections_full().is_empty(), "durable handling survives ordinary future rejection without reactions");
        assert!(engine.take_late_handled_rejections().is_empty());
        let deferred = published_deferred(&mut engine, "alreadyReported");
        let promise = deferred.promise();
        deferred.reject(engine.ctx(), reason);
        engine.run_microtasks();
        assert_eq!(engine.take_unhandled_rejections_full().len(), 1);
        assert!(engine.ctx().mark_promise_handled(&promise));
        assert!(engine.ctx().mark_promise_handled(&promise), "direct marking is idempotent");
        assert!(engine.take_late_handled_rejections().is_empty());
        native_eval(&mut engine, "alreadyReported.catch(e => { globalThis.reportedReason = e; });").ok().expect("observe rejection");
        engine.run_microtasks();
        assert!(engine.take_late_handled_rejections().is_empty(), "a direct slot assignment is not a synthetic subscription");
        assert!(matches!(native_eval(&mut engine, "reportedReason === reason").ok(), Some(Value::Bool(true))));
    }

    #[test]
    fn deferred_rejection_handled_follows_subclass_aliases_and_adoption() {
        let mut engine = crate::Engine::new();
        let promise = native_eval(&mut engine, r#"
            globalThis.reason = {marker: 13};
            class DerivedPromise extends Promise {}
            globalThis.derived = new DerivedPromise((resolve, reject) => { globalThis.rejectDerived = reject; });
            derived
        "#).ok().expect("subclass promise");
        let global = engine.ctx().global_object();
        let reject = engine.ctx().get_member(&global, "rejectDerived").ok().expect("subclass resolver");
        let alias = match reject {
            Value::Obj(object) => {
                let object = object.borrow();
                let crate::value::Callable::Resolver(cell, _) = &object.call else { panic!("native resolver") };
                cell.promise.clone()
            }
            _ => panic!("resolver object"),
        };
        assert!(engine.ctx().mark_promise_handled(&alias));
        assert_eq!(engine.ctx().promise_is_handled(&promise), Some(true));
        native_eval(&mut engine, "rejectDerived(reason);").ok().expect("forwarding resolver");
        assert!(engine.ctx().mark_promise_handled(&alias), "settled alias still resolves to its actual promise");
        assert_eq!(engine.ctx().promise_is_handled(&promise), Some(true));
        assert!(engine.take_unhandled_rejections_full().is_empty());
        native_eval(&mut engine,
            "globalThis.adopted = Promise.resolve(derived); globalThis.forwardedCatch; derived.catch(e => { forwardedCatch = e; });").ok().expect("adoption");
        engine.run_microtasks();
        let reported = engine.take_unhandled_rejections_full();
        assert_eq!(reported.len(), 1, "adopting a handled source creates an independent rejection");
        let global = engine.ctx().global_object();
        let adopted = engine.ctx().get_member(&global, "adopted").ok().expect("adopted promise");
        assert_eq!(reported[0].0.object_identity(), adopted.object_identity());
        assert!(matches!(native_eval(&mut engine, "forwardedCatch === reason").ok(), Some(Value::Bool(true))));
        // Native constructor graft aliases remain valid after settlement.
        let source_alias = match &promise {
            Value::Obj(object) => {
                let object = object.borrow();
                let crate::value::Callable::Promise(slot) = &object.call else { panic!("subclass slot") };
                slot.clone()
            }
            _ => panic!("subclass object"),
        };
        assert!(source_alias.handled, "slot snapshots preserve durable handling state");
    }

    #[test]
    fn deferred_rejection_handled_isolated_across_creating_realms_and_bounded_slot() {
        let mut engine = crate::Engine::new();
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let parent_deferred = published_deferred(&mut engine, "parentPromise");
        let parent_promise = parent_deferred.promise();
        let child_deferred = engine.ctx().with_host_realm(&child, Deferred::new).expect("child deferred");
        let child_promise = child_deferred.promise();
        child_deferred.reject_handled(engine.ctx(), Value::from_string("child handled".into()));
        assert_eq!(engine.ctx().promise_is_handled(&child_promise), Some(true));
        assert_eq!(engine.ctx().promise_is_handled(&parent_promise), Some(false));
        parent_deferred.reject(engine.ctx(), OpError::new("Error", "parent unhandled"));
        let reported = engine.take_unhandled_rejections_with_realm();
        assert_eq!(reported.len(), 1);
        assert!(reported[0].0.same_realm(&parent));
        assert_eq!(reported[0].1.object_identity(), parent_promise.object_identity());
        let child_unhandled = engine.ctx().with_host_realm(&child, Deferred::new).expect("second child deferred");
        let child_unhandled_promise = child_unhandled.promise();
        child_unhandled.reject(engine.ctx(), OpError::new("Error", "child unhandled"));
        let reported = engine.take_unhandled_rejections_with_realm();
        assert_eq!(reported.len(), 1);
        assert!(reported[0].0.same_realm(&child));
        assert_eq!(reported[0].1.object_identity(), child_unhandled_promise.object_identity());
        assert_eq!(engine.ctx().promise_is_handled(&Value::Num(7.0)), None);
        assert!(!engine.ctx().mark_promise_handled(&Value::Num(7.0)));
        let slot_bytes = std::mem::size_of::<crate::eval::promise_fast::PromiseSlot>();
        assert!(slot_bytes <= 120, "PromiseSlot uses {slot_bytes} bytes");
        eprintln!("PromiseSlot bytes with durable handling: {slot_bytes}");
    }

    #[test]
    fn native_webidl_descriptor_policy_is_explicit_and_preserves_ordinary_classes() {
        let mut engine = crate::Engine::new();
        engine.define_class::<WebIdlDescriptor>();
        engine.define_class::<OrdinaryDescriptor>();
        let result = native_eval(&mut engine, r#"
            const idl = WebIdlDescriptor.prototype;
            const ordinary = OrdinaryDescriptor.prototype;
            const idlAttribute = Object.getOwnPropertyDescriptor(idl, 'value');
            const idlOperation = Object.getOwnPropertyDescriptor(idl, 'operation');
            const idlStatic = Object.getOwnPropertyDescriptor(WebIdlDescriptor, 'staticOperation');
            const ordinaryAttribute = Object.getOwnPropertyDescriptor(ordinary, 'value');
            const ordinaryOperation = Object.getOwnPropertyDescriptor(ordinary, 'operation');
            const object = new WebIdlDescriptor(4);
            object.value = 9;
            idlAttribute.enumerable && idlAttribute.configurable &&
            typeof idlAttribute.get === 'function' && typeof idlAttribute.set === 'function' &&
            idlOperation.enumerable && idlOperation.configurable && idlOperation.writable &&
            idlStatic.enumerable && idlStatic.configurable && idlStatic.writable &&
            !ordinaryAttribute.enumerable && ordinaryAttribute.configurable &&
            !ordinaryOperation.enumerable && ordinaryOperation.configurable && ordinaryOperation.writable &&
            !Object.getOwnPropertyDescriptor(idl, 'constructor').enumerable &&
            !Object.getOwnPropertyDescriptor(WebIdlDescriptor, 'prototype').enumerable &&
            !Object.getOwnPropertyDescriptor(globalThis, 'WebIdlDescriptor').enumerable &&
            object.operation() === 9 && WebIdlDescriptor.staticOperation() === 42
        "#).ok().expect("descriptor checks execute");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[lumen_bind::module(name = "lazyFixtures")]
    mod lazy_fixtures {
        #[class(name = "LazyThing", hint(js(webidl)))]
        pub struct LazyThing;

        #[methods]
        impl LazyThing {
            #[constant]
            const FIRST: u16 = 1;
            #[constant(name = "SECOND")]
            const SECOND_VALUE: u16 = 2;

            #[constructor]
            fn new() -> Self {
                Self
            }
        }

        #[op(hint(js(webidl)))]
        #[allow(non_snake_case)]
        pub fn lazyDouble(x: i32) -> i32 {
            x * 2
        }

        #[op(hint(js(webidl)))]
        #[allow(non_snake_case)]
        pub fn lazyAssigned(x: i32) -> i32 {
            x
        }

        #[op(hint(js(webidl)))]
        #[allow(non_snake_case)]
        pub fn lazyDeleted(x: i32) -> i32 {
            x
        }

        #[op(hint(js(webidl)))]
        #[allow(non_snake_case)]
        pub fn lazyPresent(x: i32) -> i32 {
            x
        }

        #[constant(name = "LAZY_LIMIT", enumerable)]
        const LIMIT: u32 = 7;
    }

    #[test]
    fn lazy_module_globals_become_the_eager_descriptors_on_first_access() {
        let mut engine = crate::Engine::new();
        native_eval(&mut engine, "globalThis.lazyPresent = 'canonical'")
            .ok()
            .expect("preexisting global");
        engine
            .define_lazy_globals::<lazy_fixtures::Module>()
            .ok()
            .expect("lazy install");
        let global = engine.ctx().global_object();
        let global = global.as_obj().expect("global object").clone();
        for (name, enumerable) in [("LazyThing", false), ("lazyDouble", true), ("LAZY_LIMIT", true)] {
            let borrowed = global.borrow();
            let property = borrowed.props.get(name).expect("lazy global is published");
            assert!(property.accessor() && property.configurable(), "{name}");
            assert_eq!(property.enumerable(), enumerable, "{name}");
        }
        let result = native_eval(
            &mut engine,
            r#"
            const own = (name) => Object.getOwnPropertyDescriptor(globalThis, name);
            const untouched = own('lazyPresent').value === 'canonical' && !('get' in own('lazyPresent'));

            const Thing = LazyThing;
            const classAfter = own('LazyThing');
            const stable = classAfter.value === Thing && LazyThing === Thing &&
                classAfter.writable && !classAfter.enumerable && classAfter.configurable;
            const opAfter = own('lazyDouble');
            const opReady = lazyDouble(4) === 8 && typeof opAfter.value === 'function' &&
                opAfter.writable && opAfter.enumerable && opAfter.configurable &&
                opAfter.value.name === 'lazyDouble';
            const constantAfter = own('LAZY_LIMIT');
            const constantReady = LAZY_LIMIT === 7 && constantAfter.writable &&
                constantAfter.enumerable && constantAfter.configurable;

            globalThis.lazyAssigned = 'mine';
            const assigned = own('lazyAssigned');
            const assignReplaces = assigned.value === 'mine' && assigned.writable &&
                assigned.enumerable && assigned.configurable;
            delete globalThis.lazyDeleted;
            const deleteWins = own('lazyDeleted') === undefined && typeof lazyDeleted === 'undefined';

            const redefined = (() => {
                Object.defineProperty(globalThis, 'LazyThing', { value: 1, configurable: true });
                return LazyThing === 1;
            })();
            untouched && stable && opReady && constantReady &&
                assignReplaces && deleteWins && redefined
            "#,
        )
        .ok()
        .expect("lazy descriptor checks execute");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn class_constants_live_on_constructor_and_prototype_as_web_idl_constants() {
        let mut engine = crate::Engine::new();
        engine.define_class::<lazy_fixtures::LazyThing>();
        let result = native_eval(
            &mut engine,
            r#"
            const check = (target) => {
                const first = Object.getOwnPropertyDescriptor(target, 'FIRST');
                const second = Object.getOwnPropertyDescriptor(target, 'SECOND');
                return first.value === 1 && second.value === 2 &&
                    first.enumerable && second.enumerable &&
                    !first.writable && !second.writable &&
                    !first.configurable && !second.configurable;
            };
            LazyThing.FIRST = 9;
            check(LazyThing) && check(LazyThing.prototype) &&
                new LazyThing().FIRST === 1 && LazyThing.FIRST === 1 &&
                !Object.hasOwn(LazyThing, 'SECOND_VALUE')
            "#,
        )
        .ok()
        .expect("class constant checks execute");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_override_builtins_named_properties_keep_own_expandos() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeOverrideNamedWrapper>();
        let result = native_eval(
            &mut engine,
            r#"
                const shadowed = new NativeOverrideNamedWrapper();
                shadowed.enableNamed();
                const descriptor = Object.getOwnPropertyDescriptor(shadowed, 'collision');
                const blockedDefine = (() => {
                    try { Object.defineProperty(shadowed, 'collision', {value: 'define'}); }
                    catch (error) { return error instanceof TypeError; }
                    return false;
                })();
                const setIsRejected = (() => {
                    try {
                        (function() { 'use strict'; shadowed.collision = 'assigned'; })();
                    } catch (error) { return error instanceof TypeError; }
                    return false;
                })();
                const deleteIsRejected = delete shadowed.collision === false &&
                    shadowed.collision === 'named:collision';
                const own = new NativeOverrideNamedWrapper();
                Object.defineProperty(own, 'collision', {
                    value: 'expando', writable: true, enumerable: true, configurable: true
                });
                own.enableNamed();
                shadowed.collision === 'named:collision' &&
                    descriptor.value === 'named:collision' && descriptor.configurable &&
                    !descriptor.writable && !descriptor.enumerable &&
                    Object.getOwnPropertyNames(shadowed).includes('collision') &&
                    !Object.keys(shadowed).includes('collision') &&
                    blockedDefine && setIsRejected && deleteIsRejected &&
                    own.collision === 'expando' &&
                    Object.getOwnPropertyDescriptor(own, 'collision').writable
            "#,
        );
        assert!(matches!(result, Ok(Value::Bool(true))));
    }

    #[test]
    fn author_proxies_do_not_inherit_native_receiver_brands() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeReceiverBase>();
        engine.define_class::<NativeReceiverDerived>();
        let result = native_eval(
            &mut engine,
            r#"
                const base = new NativeReceiverBase(11);
                const baseProxy = new Proxy(base, {});
                let baseProxyRejected = false;
                try { void baseProxy.marker; }
                catch (error) { baseProxyRejected = error instanceof TypeError; }

                const derived = new NativeReceiverDerived(17);
                const derivedWorks = derived.marker === 17;
                const derivedProxy = new Proxy(derived, {});
                let derivedProxyRejected = false;
                try { void derivedProxy.marker; }
                catch (error) { derivedProxyRejected = error instanceof TypeError; }

                baseProxyRejected && derivedWorks && derivedProxyRejected
            "#,
        )
        .ok()
        .expect("native brand checks execute");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn indexed_native_wrappers_keep_their_brand_but_nested_author_proxies_do_not() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeIndexedWrapper>();
        let result = native_eval(
            &mut engine,
            r#"
                const authorTarget = new NativeIndexedWrapper();
                const authorProxy = new Proxy(authorTarget, {});
                globalThis.Reflect = {
                    get() { throw new Error('mutable Reflect.get used'); },
                    has() { throw new Error('mutable Reflect.has used'); },
                    set() { throw new Error('mutable Reflect.set used'); },
                    defineProperty() { throw new Error('mutable Reflect.defineProperty used'); },
                    deleteProperty() { throw new Error('mutable Reflect.deleteProperty used'); },
                    ownKeys() { throw new Error('mutable Reflect.ownKeys used'); },
                    getOwnPropertyDescriptor() { throw new Error('mutable Reflect descriptor used'); }
                };
                globalThis.Proxy = function() { throw new Error('mutable Proxy used'); };
                const indexed = new NativeIndexedWrapper();
                let authorProxyRejected = false;
                try { authorProxy.sum(); }
                catch (error) { authorProxyRejected = error instanceof TypeError; }

                // Native wrapper creation after poisoning still uses the engine intrinsic.
                let mutableProxyRejected = false;
                try { new Proxy({}, {}); }
                catch (error) { mutableProxyRejected = /mutable Proxy/.test(error.message); }

                const ownKeysWork = Object.keys(indexed).join(',') === '0,1,2';
                const descriptor = Object.getOwnPropertyDescriptor(indexed, '0');
                const trapsWork = 0 in indexed && !(9 in indexed) &&
                    descriptor.value === 5 && descriptor.enumerable;
                indexed.extra = 7;
                const setWorks = indexed.extra === 7;
                const defineWorks = Object.defineProperty(indexed, 'defined', {
                    value: 11, configurable: true, enumerable: true
                }) === indexed && indexed.defined === 11;
                const deleteWorks = delete indexed.extra && !('extra' in indexed);

                indexed instanceof NativeIndexedWrapper &&
                indexed.constructor === NativeIndexedWrapper &&
                Object.getPrototypeOf(indexed) === NativeIndexedWrapper.prototype &&
                indexed.length === 3 && indexed[0] === 5 && indexed[2] === 13 &&
                indexed.sum() === 26 && authorProxyRejected && mutableProxyRejected &&
                ownKeysWork && trapsWork && setWorks && defineWorks && deleteWorks
            "#,
        )
        .ok()
        .expect("indexed native wrapper checks execute");
        assert!(matches!(result, Value::Bool(true)));

        let indexed = native_eval(&mut engine, "new NativeIndexedWrapper()")
            .ok()
            .expect("construct another indexed wrapper");
        assert_eq!(
            engine
                .ctx()
                .with_instance::<NativeIndexedWrapper, _>(&indexed, |value| value.values.clone())
                .expect("trusted indexed proxy projects to its native value"),
            vec![5, 8, 13]
        );
    }

    #[test]
    fn native_indexed_single_probe_preserves_live_membership_and_missing_property_receivers() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeSingleProbeIndexed>();
        let result = native_eval(&mut engine, r#"
            const indexed = new NativeSingleProbeIndexed();
            const check = (value,message) => { if(!value) throw Error(message); };
            check(indexed[0]===5 && indexed[2]===13 && indexed.lengthChecks===0, 'one getter probe without length dispatch');
            Object.defineProperty(indexed, 'length', {get(){throw Error('author length getter called')}});
            indexed['01']='expando'; indexed['-0']='minus-zero';
            Object.defineProperty(NativeSingleProbeIndexed.prototype, '8', {
                configurable:true, get(){ check(this===indexed,'inherited getter receiver'); this.push(21); return this[3]; }
            });
            const before=indexed.lengthChecks;
            check(indexed[8]===21 && indexed[3]===21 && indexed[9]===undefined, 'live membership and missing fallback');
            check(indexed['01']==='expando' && indexed['-0']==='minus-zero', 'noncanonical numeric expandos');
            check(indexed.lengthChecks===before,'value probes do not dispatch length');
            check(Object.keys(indexed).includes('3') && Object.getOwnPropertyDescriptor(indexed,'3').value===21 && (3 in indexed) && (8 in indexed), 'supported index reflection and inherited presence');
            true
        "#).ok().expect("native indexed probe regression executes");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_indexed_reads_decline_changed_handler_and_foreign_trap_realm() {
        let mut engine=crate::Engine::new();engine.define_class::<NativeSingleProbeIndexed>();
        let indexed=native_eval(&mut engine,"globalThis.indexed=new NativeSingleProbeIndexed();indexed").ok().expect("indexed fixture");
        let pointer=Gc::as_ptr(indexed.as_obj().expect("native wrapper")) as usize;
        let handler=engine.ctx().proxies.get(&pointer).expect("registered native proxy").1.clone();
        let global=engine.ctx().global_object();
        engine.ctx().member_set(&global,"internalHandler",handler).unwrap_or_else(|_|panic!("install test handler"));
        let result=native_eval(&mut engine,r#"
            globalThis.originalGet=internalHandler.get;
            internalHandler.get=function(target,key,receiver){if(receiver!==indexed)throw Error('changed receiver');return 99;};
            const changed=indexed[0]===99;
            Object.defineProperty(internalHandler,'get',{configurable:true,get(){throw Error('handler getter');}});
            let threw=false;try{indexed.length;}catch(error){threw=error.message==='handler getter';}
            Object.defineProperty(internalHandler,'get',{configurable:true,writable:true,value:originalGet});
            changed && threw && indexed[0]===5 && indexed.length===3
        "#).ok().expect("changed handler follows generic trap path");
        assert!(matches!(result,Value::Bool(true)));
        let error=engine.ctx().member_get(&global,"TypeError").unwrap_or_else(|_|panic!("original realm error constructor"));
        let foreign=engine.ctx().create_host_realm();
        engine.ctx().with_host_realm(&foreign,|ctx|{
            let global=ctx.global_object();
            ctx.member_set(&global,"indexed",indexed.clone()).unwrap_or_else(|_|panic!("foreign native fixture"));
            ctx.member_set(&global,"MainTypeError",error).unwrap_or_else(|_|panic!("foreign error fixture"));
        }).expect("enter foreign realm");
        let result=engine.eval_value_in_host_realm(&foreign,r#"
            let valid=indexed[0]===5 && indexed.length===3;
            let branded=false;try{Reflect.get(indexed,'length',{});}catch(error){branded=error instanceof MainTypeError && !(error instanceof TypeError);}
            valid && branded
        "#,false).expect("foreign realm parses").ok().expect("foreign trap execution");
        assert!(matches!(result,Value::Bool(true)),"foreign native trap retains its original realm error intrinsics");
    }

    #[test]
    fn native_indexed_hot_reads_preserve_author_shadowing_and_proxy_semantics() {
        let script=r#"(()=>{
            const indexed=new NativeSingleProbeIndexed();
            let inherited=0,lengthReads=0,proxyReads=0;
            Object.defineProperty(NativeSingleProbeIndexed.prototype,'8',{configurable:true,get(){
                if(this!==indexed)throw Error('inherited receiver');inherited++;return this[0];
            }});
            Object.defineProperty(indexed,'00',{value:31,writable:false,configurable:false});
            Object.defineProperty(indexed,'4294967295',{value:37,writable:false,configurable:false});
            const author=new Proxy(indexed,{get(target,key,receiver){
                proxyReads++;return Reflect.get(target,key,receiver);
            }});
            function scan(o){let sum=0;for(let i=0;i<o.length;i++)sum+=o[i];return sum;}
            let sum=0;
            for(let n=0;n<2048;n++)sum+=scan(indexed);
            indexed.push(21);
            Object.defineProperty(indexed,'length',{configurable:true,get(){
                if(this!==indexed)throw Error('length receiver');lengthReads++;return 1;
            }});
            for(let n=0;n<2048;n++)sum+=scan(indexed)+indexed[8]+author[1];
            let borrowed=false;try{Reflect.get(indexed,'length',{});}catch(error){borrowed=error.message==='length receiver';}
            const revocable=Proxy.revocable(indexed,{});revocable.revoke();
            let revoked=false;try{revocable.proxy[0];}catch(error){revoked=error instanceof TypeError;}
            const descriptor=Object.getOwnPropertyDescriptor(indexed,'0');
            return JSON.stringify([sum,inherited,lengthReads,proxyReads,indexed['00'],indexed['4294967295'],
                indexed[3],indexed[99]===undefined,borrowed,revoked,descriptor.value,descriptor.enumerable]);
        })()"#;
        let mut outcomes=Vec::new();
        for mode in [crate::JitMode::Disabled,crate::JitMode::Eager] {
            let mut engine=crate::Engine::new();
            engine.set_tier(crate::bytecode::Tier::Bytecode);engine.set_tier_threshold(0);engine.set_jit_mode(mode);
            engine.define_class::<NativeSingleProbeIndexed>();
            let result=native_eval(&mut engine,script).ok().expect("indexed reads preserve getters and proxies");
            outcomes.push(engine.ctx().coerce_string(&result).unwrap_or_else(|_|panic!("serialize indexed read result")));
            if mode==crate::JitMode::Eager {assert!(engine.jit_stats().executed_entries>0);}
        }
        assert_eq!(outcomes[0],outcomes[1]);
        assert_eq!(outcomes[0].as_ref(),"[90112,2048,4096,2048,31,37,21,true,true,true,5,true]");
    }

    #[test]
    fn hot_native_indexed_reads_keep_membership_and_inherited_getter_receiver() {
        let mut engine = crate::Engine::new();
        engine.set_tier(crate::bytecode::Tier::Bytecode);
        engine.set_tier_threshold(0);
        engine.set_jit_mode(crate::JitMode::Eager);
        engine.define_class::<NativeSingleProbeIndexed>();
        let result = native_eval(&mut engine, r#"
            const indexed=new NativeSingleProbeIndexed();
            Object.defineProperty(indexed,'length',{get(){throw Error('author length read');}});
            let getterCalls=0;
            Object.defineProperty(NativeSingleProbeIndexed.prototype,'8',{
                configurable:true,get(){
                    if(this!==indexed)throw Error('wrong inherited receiver');
                    getterCalls++;return this[0];
                }
            });
            function read(o,k){return o[k];}
            let sum=0;
            for(let n=0;n<2048;n++)sum+=read(indexed,0)+read(indexed,8);
            indexed.push(21);
            sum===20480 && getterCalls===2048 && read(indexed,3)===21 &&
                read(indexed,99)===undefined && indexed.lengthChecks===0
        "#).ok().expect("hot native indexed reads execute");
        assert!(matches!(result, Value::Bool(true)));
        assert!(engine.jit_stats().executed_entries > 0);
    }

    #[test]
    fn native_receiver_guards_release_multiple_borrows_after_return_and_unwind() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeReceiverBase>();
        let first = native_eval(&mut engine, "new NativeReceiverBase(11)").ok().unwrap();
        let second = native_eval(&mut engine, "new NativeReceiverBase(22)").ok().unwrap();
        let mut members: Vec<FnItem<JsHost>> = Vec::new();
        NativeReceiverBase::members(&mut members);
        let desc = members[0].desc;
        {
            let cx = ArgCx::new(engine.ctx(), &first, &[], desc);
            assert_eq!(cx.class_ref::<NativeReceiverBase>(&first, Slot::THIS).ok().unwrap().marker, 11);
            assert_eq!(cx.class_ref::<NativeReceiverBase>(&second, Slot::THIS).ok().unwrap().marker, 22);
        }
        for instance in [&first, &second] {
            engine.ctx().with_instance_mut::<NativeReceiverBase, _>(instance, |value| value.marker += 1)
                .ok().expect("all retained shared borrows released on return");
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cx = ArgCx::new(engine.ctx(), &first, &[], desc);
            let value = cx.class_mut::<NativeReceiverBase>(&first, Slot::THIS).ok().unwrap();
            value.marker = 33;
            panic!("native operation unwind");
        }));
        assert!(result.is_err());
        engine.ctx().with_instance_mut::<NativeReceiverBase, _>(&first, |value| {
            assert_eq!(value.marker, 33);
            value.marker = 44;
        }).ok().expect("native exclusive borrow released on unwind");
    }

    #[test]
    fn native_indexed_keys_preserve_canonical_spelling_and_author_expandos() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeIndexedWrapper>();
        let result = native_eval(&mut engine, r#"(() => {
            const list = new NativeIndexedWrapper();
            for (const key of ['01', '+1', '-0', '1.0', '4294967295', '18446744073709551616']) {
                list[key] = 'author';
                if (list[key] !== 'author') return false;
            }
            return list[0] === 5 && list[1] === 8 && list.length === 3;
        })()"#).ok().expect("canonical key regression executes");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn indexed_native_setters_drive_assignment_and_reflected_descriptors() {
        let mut engine = crate::Engine::new();
        engine.define_class::<NativeMutableIndexedWrapper>();
        let result = native_eval(
            &mut engine,
            r#"
                const value = new NativeMutableIndexedWrapper();
                value[0] = 7;
                value[1] = 11;
                const descriptor = Object.getOwnPropertyDescriptor(value, '0');
                Object.defineProperty(value, '1', {
                    value: 13, configurable: true, enumerable: true, writable: true
                });
                let outOfRange = false;
                try { value[3] = 17; }
                catch (error) { outOfRange = error instanceof RangeError; }
                value.length === 2 && value[0] === 7 && value[1] === 13 &&
                descriptor.enumerable && descriptor.configurable && descriptor.writable &&
                outOfRange && Object.keys(value).join(',') === '0,1'
            "#,
        )
        .ok()
        .expect("indexed setter checks execute");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_iterable_conversion_uses_protocol_and_closes_abrupt_conversions() {
        let mut engine = crate::Engine::new();
        let set = native_eval(
            &mut engine,
            "Array.from = () => { throw new Error('author Array.from'); }; new Set([1, 2])",
        )
        .ok()
        .expect("Set source");
        let values = engine
            .ctx()
            .iterable_to_list(&set, 2)
            .ok()
            .expect("native Set iteration");
        assert!(matches!(
            values.as_slice(),
            [Value::Num(1.0), Value::Num(2.0)]
        ));
        let array_like = native_eval(&mut engine, "({0: 1, length: 1})")
            .ok()
            .expect("array-like source");
        assert!(engine.ctx().iterable_to_list(&array_like, 2).is_err());
        let generator = native_eval(&mut engine, "globalThis.closed = false; globalThis.visited = []; (function*() { try { visited.push(1); yield 1; visited.push(2); yield 'bad'; visited.push(3); yield 3; } finally { closed = true; } })()")
            .ok().expect("generator source");
        let result = engine.ctx().convert_iterable(&generator, 8, |_, value| {
            if matches!(value, Value::Num(_)) {
                Ok(value)
            } else {
                Err(OpError::type_error("numeric item required"))
            }
        });
        assert!(result.is_err());
        assert!(matches!(
            native_eval(&mut engine, "closed && visited.join(',') === '1,2'").ok(),
            Some(Value::Bool(true))
        ));
        let throwing = native_eval(&mut engine, "globalThis.iteratorFailure = {}; ({[Symbol.iterator]() { return this; }, next() { throw iteratorFailure; }, return() { throw new Error('close failure'); }})")
            .ok().expect("throwing iterator source");
        let failure = engine
            .ctx()
            .iterable_to_list(&throwing, 8)
            .err()
            .expect("iteration must reject");
        let failure = failure.to_value(engine.ctx());
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .member_set(&global, "actualFailure", failure)
            .ok()
            .expect("record exception");
        assert!(matches!(
            native_eval(&mut engine, "actualFailure === iteratorFailure").ok(),
            Some(Value::Bool(true))
        ));
        let unbounded = native_eval(&mut engine, "globalThis.limitClosed = false; (function*() { try { while (true) yield 1; } finally { limitClosed = true; } })()")
            .ok().expect("unbounded generator source");
        assert!(engine.ctx().iterable_to_list(&unbounded, 2).is_err());
        assert!(matches!(
            native_eval(&mut engine, "limitClosed").ok(),
            Some(Value::Bool(true))
        ));
    }

    #[test]
    fn native_promise_coercion_reuses_intrinsics_and_preserves_constructor_errors() {
        let mut engine = crate::Engine::new();
        let promise = native_eval(&mut engine, "globalThis.savedPromise = Promise; globalThis.p = Promise.resolve(1); globalThis.Promise = function() { throw new Error('author Promise'); }; p")
            .ok().expect("promise source");
        let coerced = engine
            .ctx()
            .coerce_promise(promise)
            .ok()
            .expect("intrinsic promise coercion");
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .member_set(&global, "coerced", coerced)
            .ok()
            .expect("record promise");
        assert!(matches!(
            native_eval(
                &mut engine,
                "coerced === p && coerced instanceof savedPromise"
            )
            .ok(),
            Some(Value::Bool(true))
        ));
        let poisoned = native_eval(&mut engine, "globalThis.constructorFailure = {}; Object.defineProperty(p, 'constructor', {get() { throw constructorFailure; }}); p")
            .ok().expect("poisoned promise source");
        let failure = engine
            .ctx()
            .coerce_promise(poisoned)
            .err()
            .expect("constructor getter must reject");
        let failure = failure.to_value(engine.ctx());
        engine
            .ctx()
            .member_set(&global, "coercionFailure", failure)
            .ok()
            .expect("record exception");
        assert!(matches!(
            native_eval(&mut engine, "coercionFailure === constructorFailure").ok(),
            Some(Value::Bool(true))
        ));
    }

    #[test]
    fn native_promise_observer_uses_intrinsics_and_preserves_constructor_errors() {
        let mut engine = crate::Engine::new();
        let rejected = native_eval(&mut engine,"globalThis.reason={}; globalThis.observed=null; globalThis.observerCalls=0; const p=Promise.reject(reason); Promise.prototype.then=()=>{throw 'author then'}; p")
            .ok().expect("native rejected promise");
        let ok = engine.ctx().new_native_fn("unexpectedFulfillment",1,Rc::new(|_,_,_|Err(Value::str("unexpected fulfillment"))));
        let failed = engine.ctx().new_native_fn("observeNativeRejection",1,Rc::new(|ctx,_,args| {
            let global = ctx.global_object();
            ctx.member_set(&global,"observed",args.first().cloned().unwrap_or(Value::Undefined))?;
            let calls = ctx.get_member(&global,"observerCalls").map_err(abrupt_value)?;
            let Value::Num(calls) = calls else { return Err(Value::str("invalid observer count")); };
            ctx.member_set(&global,"observerCalls",Value::Num(calls+1.0))?;
            Ok(Value::Undefined)
        }));
        engine.ctx().then_value(rejected,ok.clone(),failed.clone());
        while engine.run_one_job() {}
        assert!(matches!(native_eval(&mut engine,"observed===reason && observerCalls===1").ok(),Some(Value::Bool(true))));
        let poisoned = native_eval(&mut engine,"globalThis.constructorReason={}; globalThis.constructorReads=0; const q=Promise.resolve(1); Object.defineProperty(q,'constructor',{get(){constructorReads++;throw constructorReason}}); Object.defineProperty(q,'then',{get(){throw 'unexpected then lookup'}}); q")
            .ok().expect("native promise with throwing constructor");
        engine.ctx().then_value(poisoned,ok,failed);
        while engine.run_one_job() {}
        assert!(matches!(native_eval(&mut engine,"observed===constructorReason && observerCalls===2 && constructorReads===1").ok(),Some(Value::Bool(true))));
    }

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
    fn buffer_source_copy_checks_brands_ranges_and_detachment() {
        let mut engine = crate::Engine::new();
        for (source, expected) in [
            ("new Uint8Array([1,2,3,4]).buffer", Some(vec![1, 2, 3, 4])),
            ("new Uint8Array([1,2,3,4]).subarray(1,3)", Some(vec![2, 3])),
            (
                "new DataView(new Uint8Array([1,2,3,4]).buffer,1,2)",
                Some(vec![2, 3]),
            ),
            ("({get buffer(){throw 1},byteOffset:0,byteLength:4})", None),
            ("Object.create(DataView.prototype)", None),
            (
                "(()=>{let b=new ArrayBuffer(4);let v=new DataView(b);b.transfer();return v})()",
                None,
            ),
            (
                "(()=>{let b=new ArrayBuffer(4,{maxByteLength:8});let v=new DataView(b,2,2);b.resize(1);return v})()",
                None,
            ),
            ("new Uint8Array(new SharedArrayBuffer(4))", None),
        ] {
            let value = engine
                .eval_value(source)
                .expect("valid source")
                .ok()
                .expect("source completes");
            assert_eq!(
                engine.ctx().buffer_source_bytes(&value),
                expected,
                "{source}"
            );
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

    #[test]
    fn date_brand_uses_hidden_exotic_slot_and_mutators_preserve_it() {
        let mut engine = crate::Engine::new();
        let result = native_eval(
            &mut engine,
            r#"(() => {
                const getTime = Date.prototype.getTime;
                const setTime = Date.prototype.setTime;
                const fake = { '\0date_ms': 1234 };
                let fakeRejected = false;
                try { getTime.call(fake); } catch (error) { fakeRejected = error instanceof TypeError; }
                const date = new Date(1234);
                Object.setPrototypeOf(date, {});
                setTime.call(date, 5678);
                const clone = new Date(date);
                return fakeRejected && getTime.call(date) === 5678 &&
                    clone.getTime() === 5678 && Object.prototype.toString.call(date) === '[object Date]';
            })()"#,
        );
        assert!(matches!(result, Ok(Value::Bool(true))));
    }
}
