//! [`ByteStore`]: the backing bytes of a buffer, owned (`Vec`) or external, with the flags and
//! export (pin) count every facade consults.

use super::BufferError;
use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

// ---- external-memory pressure ----------------------------------------------------------------
// A collector triggered by object counts never sees buffer bytes: a loop allocating a large
// buffer per iteration creates few objects and would never collect. Owned bytes are tallied per
// thread (a ByteStore is !Send, so it is created and dropped on its owner's thread) and crossing
// the budget raises a flag the collector polls ([`gc_pressure`]) and clears ([`after_gc`]).

/// Owned bytes allowed to accumulate before a collection is requested.
const EXTERNAL_GC_MIN: usize = 256 << 20;

thread_local! {
    static TRACKED: Cell<usize> = const { Cell::new(0) };
    static NEXT_GC: Cell<usize> = const { Cell::new(EXTERNAL_GC_MIN) };
    static PRESSURE: Cell<bool> = const { Cell::new(false) };
}

fn track_add(n: usize) {
    let _ = TRACKED.try_with(|t| {
        let total = t.get().saturating_add(n);
        t.set(total);
        if total > NEXT_GC.with(Cell::get) {
            PRESSURE.with(|p| p.set(true));
        }
    });
}

fn track_sub(n: usize) {
    let _ = TRACKED.try_with(|t| t.set(t.get().saturating_sub(n)));
}

/// Owned buffer bytes alive on this thread.
pub fn tracked_bytes() -> usize {
    TRACKED.with(Cell::get)
}

/// Whether this thread's owned buffer bytes grew past the budget since the last collection.
#[inline]
pub fn gc_pressure() -> bool {
    PRESSURE.with(Cell::get)
}

/// After a collection: the next request comes once the surviving bytes double (and at least the
/// minimum budget above them).
pub fn after_gc() {
    let live = TRACKED.with(Cell::get);
    NEXT_GC.with(|n| n.set(live.saturating_mul(2).max(live.saturating_add(EXTERNAL_GC_MIN))));
    PRESSURE.with(|p| p.set(false));
}

// ---- the store ---------------------------------------------------------------------------------

const READONLY: u8 = 1;
const RESIZABLE: u8 = 2;
const DETACHED: u8 = 4;

enum Backing {
    Heap(Vec<u8>),
    External(#[allow(dead_code)] Rc<dyn Any>),
    Detached,
}

/// The bytes behind a buffer object, shared by reference (`Rc<ByteStore>`) between every view
/// and, through a bridge, between languages.
///
/// Invariants (what makes the interior mutability sound):
/// - `ptr..ptr + len` is the live byte range. For `Backing::Heap` it is the `Vec`'s buffer; for
///   `Backing::External` it is memory the owner keeps valid; when detached it is empty.
/// - `borrow` counts live [`Bytes`] guards (> 0) or marks one live [`BytesMut`] (-1). Slices are
///   only handed out through those guards, through [`Lend`]s (or [`ByteStore::as_ptr`] for
///   callers that manage the aliasing themselves), and `backing`, `ptr` and `len` change only
///   while `borrow == 0`, so no reference into the bytes outlives a resize or detach.
/// - `lends` / `mut_lends` count the live [`Lend`]s of the current bytes. A lend pins the store,
///   so the length never changes under it. An access that would alias a lent slice (a write
///   while any lend is live, any access while a mutable lend is live) first *unshares*: the
///   bytes are copied to a new allocation that becomes current, and the lent one is retired
///   (kept alive, untouched by the store) until its last lend ends.
/// - The store is `!Send`/`!Sync` (it holds `Cell`s and an `Rc`), so all of this is single-threaded.
pub struct ByteStore {
    ptr: Cell<*mut u8>,
    len: Cell<usize>,
    borrow: Cell<i32>,
    pins: Cell<u32>,
    lends: Cell<u32>,
    mut_lends: Cell<u32>,
    flags: Cell<u8>,
    max_len: Cell<usize>,
    backing: UnsafeCell<Backing>,
    /// Lent allocations replaced by an unshare (empty unless a lend outlived a conflicting
    /// access); `gen` numbers the current allocation.
    retired: UnsafeCell<Vec<Retired>>,
    gen: Cell<u32>,
}

/// A lent allocation that is no longer the store's current bytes.
struct Retired {
    gen: u32,
    /// Keeps the memory alive.
    backing: Backing,
    ptr: *mut u8,
    len: usize,
    lends: u32,
    /// The bytes as they were when retired, when a mutable lend was live: on its release, the
    /// bytes it changed since are carried over to the current allocation.
    snap: Option<Vec<u8>>,
}

impl ByteStore {
    /// A fixed-length, writable store owning `v`.
    pub fn new(mut v: Vec<u8>) -> ByteStore {
        track_add(v.capacity());
        let len = v.len();
        ByteStore {
            ptr: Cell::new(v.as_mut_ptr()),
            len: Cell::new(len),
            borrow: Cell::new(0),
            pins: Cell::new(0),
            lends: Cell::new(0),
            mut_lends: Cell::new(0),
            flags: Cell::new(0),
            max_len: Cell::new(len),
            backing: UnsafeCell::new(Backing::Heap(v)),
            retired: UnsafeCell::new(Vec::new()),
            gen: Cell::new(0),
        }
    }

    /// `n` zero bytes.
    pub fn zeroed(n: usize) -> ByteStore {
        ByteStore::new(vec![0u8; n])
    }

    /// A fixed-length view of `len` bytes at `ptr`, kept valid by `owner`; no copy.
    ///
    /// # Safety
    /// `ptr..ptr + len` must stay readable and writable, and not be aliased by live Rust
    /// references, for as long as `owner` is alive.
    pub unsafe fn external(ptr: *mut u8, len: usize, owner: Rc<dyn Any>) -> ByteStore {
        ByteStore {
            ptr: Cell::new(ptr),
            len: Cell::new(len),
            borrow: Cell::new(0),
            pins: Cell::new(0),
            lends: Cell::new(0),
            mut_lends: Cell::new(0),
            flags: Cell::new(0),
            max_len: Cell::new(len),
            backing: UnsafeCell::new(Backing::External(owner)),
            retired: UnsafeCell::new(Vec::new()),
            gen: Cell::new(0),
        }
    }

    /// Make the store resizable up to `max` bytes (at least its current length).
    pub fn with_max_len(self, max: usize) -> ByteStore {
        self.max_len.set(max.max(self.len()));
        self.flags.set(self.flags.get() | RESIZABLE);
        self
    }

    /// Make the store resizable without a practical bound (a Python `bytearray`).
    pub fn growable(self) -> ByteStore {
        self.with_max_len(isize::MAX as usize)
    }

    /// Mark the contents immutable (a Python `bytes`, an immutable JS `ArrayBuffer`).
    pub fn readonly(self) -> ByteStore {
        self.set_readonly();
        self
    }

    /// [`readonly`](Self::readonly) on a shared store, once it has been filled.
    pub fn set_readonly(&self) {
        self.flags.set(self.flags.get() | READONLY);
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len.get()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Readonly stores refuse [`resize`](Self::resize) and [`edit`](Self::edit). Facades check it
    /// before writing through a view: [`bytes_mut`](Self::bytes_mut) does not.
    #[inline(always)]
    pub fn is_readonly(&self) -> bool {
        self.flags.get() & READONLY != 0
    }

    #[inline]
    pub fn is_resizable(&self) -> bool {
        self.flags.get() & RESIZABLE != 0
    }

    #[inline]
    pub fn is_detached(&self) -> bool {
        self.flags.get() & DETACHED != 0
    }

    /// The largest length [`resize`](Self::resize) accepts (the length itself when fixed).
    #[inline]
    pub fn max_len(&self) -> usize {
        self.max_len.get()
    }

    /// Whether the bytes belong to an external owner (see [`external`](Self::external)).
    pub fn is_external(&self) -> bool {
        // SAFETY: a shared read of the discriminant; `backing` is only replaced by methods of
        // this type, never while a reference to it is live.
        matches!(unsafe { &*self.backing.get() }, Backing::External(_))
    }

    /// Live exports (pins).
    #[inline]
    pub fn pins(&self) -> usize {
        self.pins.get() as usize
    }

    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.pins.get() != 0
    }

    /// Register an export: until the matching [`unpin`](Self::unpin) the store cannot be
    /// resized, edited or detached ([`BufferError::Pinned`]).
    pub fn pin(&self) -> Result<(), BufferError> {
        if self.is_detached() {
            return Err(BufferError::Detached);
        }
        self.pins.set(self.pins.get() + 1);
        Ok(())
    }

    /// Release one export taken with [`pin`](Self::pin).
    pub fn unpin(&self) {
        debug_assert!(self.pins.get() > 0, "unbalanced ByteStore::unpin");
        self.pins.set(self.pins.get().saturating_sub(1));
    }

    /// [`pin`](Self::pin) as a guard that unpins when dropped and keeps the store alive.
    pub fn export(self: &Rc<Self>) -> Result<Export, BufferError> {
        self.pin()?;
        Ok(Export(self.clone()))
    }

    /// Shared access to the bytes. Panics while a [`BytesMut`] is live (see
    /// [`try_bytes`](Self::try_bytes)).
    #[inline(always)]
    pub fn bytes(&self) -> Bytes<'_> {
        match self.try_bytes() {
            Ok(b) => b,
            Err(_) => borrow_panic(),
        }
    }

    #[inline(always)]
    pub fn try_bytes(&self) -> Result<Bytes<'_>, BufferError> {
        let b = self.borrow.get();
        if b < 0 {
            return Err(BufferError::Borrowed);
        }
        if self.mut_lends.get() != 0 {
            self.unshare()?;
        }
        self.borrow.set(b + 1);
        Ok(Bytes(self))
    }

    /// Exclusive access to the bytes (readonly is not enforced here). Panics while any other
    /// guard is live (see [`try_bytes_mut`](Self::try_bytes_mut)).
    #[inline(always)]
    pub fn bytes_mut(&self) -> BytesMut<'_> {
        match self.try_bytes_mut() {
            Ok(b) => b,
            Err(_) => borrow_panic(),
        }
    }

    #[inline(always)]
    pub fn try_bytes_mut(&self) -> Result<BytesMut<'_>, BufferError> {
        if self.borrow.get() != 0 {
            return Err(BufferError::Borrowed);
        }
        if self.lends.get() != 0 {
            self.unshare()?;
        }
        self.borrow.set(-1);
        Ok(BytesMut(self))
    }

    /// Lend the bytes for the length of a native call that may run script code: like an
    /// [`export`](Self::export) (the store cannot be resized, edited or detached while lent),
    /// plus a slice that stays valid and unaliased for the lend's life. Script code may still
    /// read and write the buffer in place meanwhile, as through any export: an access that
    /// would alias the slice moves the store to a copy first (see the type docs), so a shared
    /// lend keeps reading the bytes as they were, and the bytes a mutable lend changes are
    /// carried over when it ends.
    ///
    /// Fails while a [`Bytes`] / [`BytesMut`] guard conflicts, and with
    /// [`BufferError::Borrowed`] when an external store would have to be copied.
    pub fn lend(&self, mutable: bool) -> Result<Lend<'_>, BufferError> {
        if self.is_detached() {
            return Err(BufferError::Detached);
        }
        let b = self.borrow.get();
        if b < 0 || (mutable && b > 0) {
            return Err(BufferError::Borrowed);
        }
        if self.mut_lends.get() != 0 || (mutable && self.lends.get() != 0) {
            self.unshare()?;
        }
        self.pins.set(self.pins.get() + 1);
        self.lends.set(self.lends.get() + 1);
        if mutable {
            self.mut_lends.set(self.mut_lends.get() + 1);
        }
        Ok(Lend { store: self, ptr: self.ptr.get(), len: self.len.get(), gen: self.gen.get(), mutable })
    }

    /// Move the current bytes to a fresh copy, retiring the lent allocation.
    #[cold]
    #[inline(never)]
    fn unshare(&self) -> Result<(), BufferError> {
        // SAFETY: no `Bytes`/`BytesMut` guard is live (callers checked `borrow`; a guard cannot
        // coexist with a mutable lend), and lends hold raw pointers, not references to `backing`.
        let backing = unsafe { &mut *self.backing.get() };
        if !matches!(backing, Backing::Heap(_)) {
            return Err(BufferError::Borrowed);
        }
        let (ptr, len) = (self.ptr.get(), self.len.get());
        let mut copy = self.bytes_unguarded().to_vec();
        track_add(copy.capacity());
        let snap = (self.mut_lends.get() != 0).then(|| {
            track_add(copy.len());
            copy.clone()
        });
        self.ptr.set(copy.as_mut_ptr());
        let old = std::mem::replace(backing, Backing::Heap(copy));
        // SAFETY: only this type touches `retired`, and never while a reference to it is live.
        let retired = unsafe { &mut *self.retired.get() };
        retired.push(Retired { gen: self.gen.get(), backing: old, ptr, len, lends: self.lends.get(), snap });
        self.lends.set(0);
        self.mut_lends.set(0);
        self.gen.set(self.gen.get().wrapping_add(1));
        Ok(())
    }

    /// End a lend (its `Drop`).
    fn release(&self, l: &Lend<'_>) {
        self.pins.set(self.pins.get() - 1);
        if l.gen == self.gen.get() {
            self.lends.set(self.lends.get() - 1);
            if l.mutable {
                self.mut_lends.set(self.mut_lends.get() - 1);
            }
            return;
        }
        // SAFETY: as in `unshare`.
        let retired = unsafe { &mut *self.retired.get() };
        let Some(i) = retired.iter().position(|r| r.gen == l.gen) else { return };
        if l.mutable {
            if let Some(snap) = retired[i].snap.take() {
                // SAFETY: the retired allocation is alive and only this (ended) lend wrote it.
                let old = unsafe { std::slice::from_raw_parts(retired[i].ptr, retired[i].len) };
                if old != snap.as_slice() && self.borrow.get() == 0 {
                    // Writing the current bytes is a write like any other: unshare first if they
                    // are lent in turn.
                    if self.lends.get() == 0 || self.unshare().is_ok() {
                        let cur = unsafe { std::slice::from_raw_parts_mut(self.ptr.get(), self.len.get()) };
                        for ((c, o), s) in cur.iter_mut().zip(old).zip(&snap) {
                            if o != s {
                                *c = *o;
                            }
                        }
                    }
                }
                track_sub(snap.len());
            }
        }
        // `unshare` above may have pushed: find the entry again.
        let retired = unsafe { &mut *self.retired.get() };
        let i = retired.iter().position(|r| r.gen == l.gen).unwrap_or(i);
        retired[i].lends -= 1;
        if retired[i].lends == 0 {
            let r = retired.swap_remove(i);
            if let Backing::Heap(v) = &r.backing {
                track_sub(v.capacity());
            }
        }
    }

    /// The address of byte 0, for code that manages aliasing itself (compiled code, FFI). It is
    /// valid until the store is resized, edited or detached, and must not be used to create a
    /// reference that overlaps a live guard.
    #[inline(always)]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.get()
    }

    /// A copy of the bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }

    /// Copy `out.len()` bytes starting at `at` into `out`; `false` (nothing copied) when the
    /// range is out of bounds.
    #[inline]
    pub fn read_at(&self, at: usize, out: &mut [u8]) -> bool {
        let b = self.bytes();
        match at.checked_add(out.len()).and_then(|end| b.get(at..end)) {
            Some(src) => {
                out.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    /// Copy `src` to offset `at`; `false` (nothing written) when the range is out of bounds.
    #[inline]
    pub fn write_at(&self, at: usize, src: &[u8]) -> bool {
        let mut b = self.bytes_mut();
        match at.checked_add(src.len()).and_then(|end| b.get_mut(at..end)) {
            Some(dst) => {
                dst.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    fn check_reshape(&self) -> Result<(), BufferError> {
        if self.is_detached() {
            return Err(BufferError::Detached);
        }
        if self.is_readonly() {
            return Err(BufferError::ReadOnly);
        }
        if !self.is_resizable() {
            return Err(BufferError::NotResizable);
        }
        if self.is_pinned() {
            return Err(BufferError::Pinned);
        }
        if self.borrow.get() != 0 {
            return Err(BufferError::Borrowed);
        }
        Ok(())
    }

    /// The owned `Vec` (an external store is first copied to the heap: its size belongs to its
    /// owner).
    ///
    /// # Safety
    /// `borrow == 0`, and the returned reference is dropped before any other access to the store.
    #[allow(clippy::mut_from_ref)]
    unsafe fn heap_vec(&self) -> &mut Vec<u8> {
        // SAFETY: `borrow == 0` (the caller's contract), so no guard references the bytes, and
        // no other reference to `backing` exists.
        let backing = unsafe { &mut *self.backing.get() };
        if !matches!(backing, Backing::Heap(_)) {
            let copy = self.bytes_unguarded().to_vec();
            track_add(copy.capacity());
            *backing = Backing::Heap(copy);
        }
        match backing {
            Backing::Heap(v) => v,
            _ => unreachable!(),
        }
    }

    fn bytes_unguarded(&self) -> &[u8] {
        // SAFETY: the invariants above: `ptr..ptr + len` is live and no `BytesMut` exists (callers
        // hold `borrow == 0`).
        unsafe { std::slice::from_raw_parts(self.ptr.get(), self.len.get()) }
    }

    fn sync_from_vec(&self) {
        // SAFETY: called right after `heap_vec` with `borrow == 0`.
        if let Backing::Heap(v) = unsafe { &mut *self.backing.get() } {
            self.ptr.set(v.as_mut_ptr());
            self.len.set(v.len());
        }
    }

    /// Resize to `n` bytes, zero-filling growth. Fails on a detached, readonly, fixed-length,
    /// pinned or borrowed store, and with [`BufferError::TooLarge`] past [`max_len`](Self::max_len).
    pub fn resize(&self, n: usize) -> Result<(), BufferError> {
        self.check_reshape()?;
        if n > self.max_len() {
            return Err(BufferError::TooLarge);
        }
        // SAFETY: `check_reshape` established `borrow == 0`; `v` is not used past this method.
        let v = unsafe { self.heap_vec() };
        let before = v.capacity();
        v.resize(n, 0);
        let after = v.capacity();
        if after > before {
            track_add(after - before);
        } else {
            track_sub(before - after);
        }
        self.sync_from_vec();
        Ok(())
    }

    /// Run `f` on the owned `Vec` (for length-changing edits such as `bytearray` slice
    /// assignment, `insert`, `extend`). Same preconditions as [`resize`](Self::resize); `max_len`
    /// is not enforced (use `resize` for bounded stores).
    pub fn edit<R>(&self, f: impl FnOnce(&mut Vec<u8>) -> R) -> Result<R, BufferError> {
        self.check_reshape()?;
        // SAFETY: `check_reshape` established `borrow == 0`; `v` is not used past this method.
        let v = unsafe { self.heap_vec() };
        // Releases the exclusive borrow and resyncs `ptr` / `len` / the tally even when `f`
        // panics (it may have reallocated the `Vec` before unwinding).
        struct EditGuard<'a> {
            store: &'a ByteStore,
            before: usize,
        }
        impl Drop for EditGuard<'_> {
            fn drop(&mut self) {
                // SAFETY: `f` has returned or unwound, so its `&mut Vec` is gone, and `f` only
                // sees the `Vec`, so `backing` is still `Heap`.
                if let Backing::Heap(v) = unsafe { &*self.store.backing.get() } {
                    let after = v.capacity();
                    if after > self.before {
                        track_add(after - self.before);
                    } else {
                        track_sub(self.before - after);
                    }
                }
                self.store.sync_from_vec();
                self.store.borrow.set(0);
            }
        }
        let guard = EditGuard { store: self, before: v.capacity() };
        // Hold the exclusive borrow while `f` runs so a re-entrant access fails cleanly.
        self.borrow.set(-1);
        let r = f(v);
        drop(guard);
        Ok(r)
    }

    /// Whether detachment can proceed without a live pin or bytes guard.
    pub fn can_detach(&self) -> bool {
        !self.is_detached() && !self.is_pinned() && self.borrow.get() == 0
    }

    /// Detach: hand the bytes out (an external store's are copied) and leave the store empty and
    /// detached. Fails on a pinned or borrowed store, or one already detached. Readonly is a
    /// facade policy here (an immutable JS buffer is not detachable; that check precedes this).
    pub fn detach(&self) -> Result<Vec<u8>, BufferError> {
        if self.is_detached() {
            return Err(BufferError::Detached);
        }
        if self.is_pinned() {
            return Err(BufferError::Pinned);
        }
        if self.borrow.get() != 0 {
            return Err(BufferError::Borrowed);
        }
        // SAFETY: `borrow == 0`: no reference into the bytes or `backing` is live.
        let old = std::mem::replace(unsafe { &mut *self.backing.get() }, Backing::Detached);
        let v = match old {
            Backing::Heap(v) => {
                track_sub(v.capacity());
                v
            }
            _ => self.bytes_unguarded().to_vec(),
        };
        self.ptr.set(std::ptr::NonNull::dangling().as_ptr());
        self.len.set(0);
        self.max_len.set(0);
        self.flags.set(self.flags.get() | DETACHED);
        Ok(v)
    }
}

impl Drop for ByteStore {
    fn drop(&mut self) {
        if let Backing::Heap(v) = self.backing.get_mut() {
            track_sub(v.capacity());
        }
    }
}

/// Bytes lent by [`ByteStore::lend`]: a slice that stays valid and unaliased until dropped.
pub struct Lend<'a> {
    store: &'a ByteStore,
    ptr: *mut u8,
    len: usize,
    gen: u32,
    mutable: bool,
}

impl Lend<'_> {
    /// The lent bytes.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the allocation stays alive while lent (current or retired), its length is
        // pinned, and the store moves away instead of writing to it.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The lent bytes, writable (a mutable lend only).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(self.mutable, "ByteStore: shared lend used mutably");
        // SAFETY: as for `as_slice`; a mutable lend is the only accessor of its allocation (any
        // other access unshares), and `&mut self` makes the slice unique.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Lend<'_> {
    fn drop(&mut self) {
        self.store.release(self);
    }
}

impl From<Vec<u8>> for ByteStore {
    fn from(v: Vec<u8>) -> ByteStore {
        ByteStore::new(v)
    }
}

impl std::fmt::Debug for ByteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteStore")
            .field("len", &self.len())
            .field("max_len", &self.max_len())
            .field("readonly", &self.is_readonly())
            .field("resizable", &self.is_resizable())
            .field("detached", &self.is_detached())
            .field("pins", &self.pins())
            .finish()
    }
}

#[cold]
#[inline(never)]
fn borrow_panic() -> ! {
    panic!("ByteStore already borrowed")
}

/// Shared access to a store's bytes (see [`ByteStore::bytes`]).
pub struct Bytes<'a>(&'a ByteStore);

impl Deref for Bytes<'_> {
    type Target = [u8];
    #[inline(always)]
    fn deref(&self) -> &[u8] {
        // SAFETY: `borrow > 0` while this guard lives, so the range stays valid and unaliased by
        // a `&mut`.
        unsafe { std::slice::from_raw_parts(self.0.ptr.get(), self.0.len.get()) }
    }
}

impl Drop for Bytes<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.0.borrow.set(self.0.borrow.get() - 1);
    }
}

/// Exclusive access to a store's bytes (see [`ByteStore::bytes_mut`]).
pub struct BytesMut<'a>(&'a ByteStore);

impl Deref for BytesMut<'_> {
    type Target = [u8];
    #[inline(always)]
    fn deref(&self) -> &[u8] {
        // SAFETY: `borrow == -1` while this guard lives: it is the only accessor.
        unsafe { std::slice::from_raw_parts(self.0.ptr.get(), self.0.len.get()) }
    }
}

impl DerefMut for BytesMut<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `deref`; `&mut self` makes the slice unique.
        unsafe { std::slice::from_raw_parts_mut(self.0.ptr.get(), self.0.len.get()) }
    }
}

impl Drop for BytesMut<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.0.borrow.set(0);
    }
}

/// A live export of a store: pinned until dropped (see [`ByteStore::export`]).
pub struct Export(Rc<ByteStore>);

impl Export {
    pub fn store(&self) -> &Rc<ByteStore> {
        &self.0
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        self.0.unpin();
    }
}

/// A buffer object's store, held inline until something needs to share it.
///
/// Most buffers are never shared, and a separate `Rc` allocation per buffer is measurable in
/// allocation-heavy code; [`share`](Self::share) moves the store behind an `Rc` on first use.
/// Moving an unshared store is sound: its bytes live in the heap block or external memory, never
/// inside the `ByteStore` itself.
pub enum StoreSlot {
    Owned(ByteStore),
    Shared(Rc<ByteStore>),
}

impl StoreSlot {
    /// The store as an `Rc`, converting an inline store in place.
    pub fn share(&mut self) -> Rc<ByteStore> {
        if let StoreSlot::Shared(rc) = self {
            return rc.clone();
        }
        let StoreSlot::Owned(store) = std::mem::replace(self, StoreSlot::Owned(ByteStore::new(Vec::new())))
        else {
            unreachable!()
        };
        let rc = Rc::new(store);
        *self = StoreSlot::Shared(rc.clone());
        rc
    }
}

impl Deref for StoreSlot {
    type Target = ByteStore;
    #[inline(always)]
    fn deref(&self) -> &ByteStore {
        match self {
            StoreSlot::Owned(s) => s,
            StoreSlot::Shared(s) => s,
        }
    }
}

impl From<ByteStore> for StoreSlot {
    fn from(s: ByteStore) -> StoreSlot {
        StoreSlot::Owned(s)
    }
}

impl From<Rc<ByteStore>> for StoreSlot {
    fn from(s: Rc<ByteStore>) -> StoreSlot {
        StoreSlot::Shared(s)
    }
}

impl std::fmt::Debug for StoreSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_store_refuses_reshape() {
        let s = ByteStore::new(vec![1, 2, 3]);
        assert_eq!(s.len(), 3);
        assert_eq!(s.resize(5), Err(BufferError::NotResizable));
        assert_eq!(&*s.bytes(), [1, 2, 3]);
        assert!(!s.is_resizable());
        assert_eq!(s.max_len(), 3);
    }

    #[test]
    fn resizable_within_max() {
        let s = ByteStore::zeroed(2).with_max_len(4);
        s.bytes_mut()[1] = 9;
        assert_eq!(s.resize(4), Ok(()));
        assert_eq!(&*s.bytes(), [0, 9, 0, 0]);
        assert_eq!(s.resize(5), Err(BufferError::TooLarge));
        assert_eq!(s.resize(1), Ok(()));
        assert_eq!(&*s.bytes(), [0]);
    }

    #[test]
    fn slot_shares_on_demand_without_moving_bytes() {
        let mut slot = StoreSlot::from(ByteStore::new(vec![1, 2, 3]));
        let before = slot.as_ptr();
        let rc = slot.share();
        assert!(matches!(slot, StoreSlot::Shared(_)));
        assert_eq!(rc.as_ptr(), before);
        rc.write_at(0, &[9]);
        assert_eq!(slot.to_vec(), vec![9, 2, 3]);
        assert!(Rc::ptr_eq(&rc, &slot.share()));
    }

    #[test]
    fn pins_block_resize_edit_and_detach() {
        let s = Rc::new(ByteStore::zeroed(4).growable());
        let e = s.export().unwrap();
        assert!(s.is_pinned());
        assert_eq!(s.resize(8), Err(BufferError::Pinned));
        assert_eq!(s.edit(|v| v.push(1)), Err(BufferError::Pinned));
        assert_eq!(s.detach(), Err(BufferError::Pinned));
        // In-place writes stay allowed while exported.
        assert!(s.write_at(1, &[7]));
        s.pin().unwrap();
        drop(e);
        assert_eq!(s.pins(), 1);
        s.unpin();
        assert_eq!(s.edit(|v| v.extend_from_slice(&[5, 6])), Ok(()));
        assert_eq!(&*s.bytes(), [0, 7, 0, 0, 5, 6]);
    }

    #[test]
    fn readonly_refuses_reshape_not_detach() {
        let s = ByteStore::new(vec![1]).growable().readonly();
        assert!(s.is_readonly());
        assert_eq!(s.resize(0), Err(BufferError::ReadOnly));
        assert_eq!(s.edit(|v| v.clear()), Err(BufferError::ReadOnly));
        assert_eq!(s.detach(), Ok(vec![1]));
    }

    #[test]
    fn detach_moves_bytes_out() {
        let s = ByteStore::new(vec![4, 5]);
        let p = s.as_ptr();
        let v = s.detach().unwrap();
        assert_eq!(v.as_ptr(), p as *const u8, "detaching an owned store does not copy");
        assert!(s.is_detached());
        assert_eq!(s.len(), 0);
        assert!(s.bytes().is_empty());
        assert_eq!(s.detach(), Err(BufferError::Detached));
        assert_eq!(s.pin(), Err(BufferError::Detached));
        assert_eq!(s.resize(0), Err(BufferError::Detached));
    }

    #[test]
    fn borrow_flags() {
        let s = ByteStore::zeroed(2).growable();
        {
            let a = s.bytes();
            let b = s.bytes();
            assert_eq!(a.len() + b.len(), 4);
            assert!(s.try_bytes_mut().is_err());
            assert_eq!(s.resize(3), Err(BufferError::Borrowed));
            assert_eq!(s.detach(), Err(BufferError::Borrowed));
        }
        {
            let _m = s.bytes_mut();
            assert!(s.try_bytes().is_err());
        }
        assert!(s.try_bytes_mut().is_ok());
        let r = s.edit(|_| s.try_bytes().is_err());
        assert_eq!(r, Ok(true), "the store is exclusively borrowed during edit");
    }

    #[test]
    fn lends_pin_and_unshare_on_conflict() {
        let s = ByteStore::new(vec![1, 2, 3, 4]).growable();
        let a = s.lend(false).unwrap();
        assert_eq!(s.resize(8), Err(BufferError::Pinned), "a lend is an export");
        assert_eq!(&*s.bytes(), [1, 2, 3, 4], "reads share the lent bytes");
        assert_eq!(s.as_ptr() as *const u8, a.as_slice().as_ptr());
        s.bytes_mut()[0] = 9;
        assert_eq!(a.as_slice(), [1, 2, 3, 4], "a write moves the store, not the lent bytes");
        assert_eq!(&*s.bytes(), [9, 2, 3, 4]);
        let b = s.lend(false).unwrap();
        assert_eq!(b.as_slice(), [9, 2, 3, 4]);
        drop(a);
        assert_eq!(s.pins(), 1);
        drop(b);
        assert_eq!(s.pins(), 0);
        s.resize(5).unwrap();
        assert_eq!(&*s.bytes(), [9, 2, 3, 4, 0]);
    }

    #[test]
    fn mutable_lend_merges_its_writes() {
        let s = ByteStore::new(vec![0; 4]).growable();
        let mut m = s.lend(true).unwrap();
        m.as_mut_slice()[0] = 1;
        assert_eq!(&*s.bytes(), [1, 0, 0, 0], "a read sees the writes so far");
        m.as_mut_slice()[1] = 2;
        s.bytes_mut()[3] = 7;
        assert_eq!(m.as_slice(), [1, 2, 0, 0]);
        drop(m);
        assert_eq!(&*s.bytes(), [1, 2, 0, 7], "both sides' writes survive");
        // A shared and a mutable lend of one store in one call never alias.
        let r = s.lend(false).unwrap();
        let mut w = s.lend(true).unwrap();
        w.as_mut_slice()[0] = 5;
        assert_eq!((r.as_slice()[0], w.as_slice()[0]), (1, 5));
        drop((r, w));
        assert_eq!(&*s.bytes(), [5, 2, 0, 7]);
        assert_eq!(s.pins(), 0);
        let _g = s.bytes();
        assert_eq!(s.lend(true).err(), Some(BufferError::Borrowed));
    }

    #[test]
    fn edit_panic_releases_and_resyncs() {
        let s = ByteStore::zeroed(2).growable();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = s.edit(|v| {
                v.extend(std::iter::repeat_n(7, 4096));
                panic!("edit closure failed");
            });
        }));
        assert!(r.is_err());
        assert_eq!(s.len(), 4098, "len follows the reallocated Vec");
        assert_eq!(s.bytes()[4097], 7);
        assert!(s.try_bytes_mut().is_ok(), "the exclusive borrow was released");
        s.resize(1).unwrap();
    }

    #[test]
    fn external_bytes_alias_until_resized() {
        let owner: Rc<std::cell::RefCell<Vec<u8>>> = Rc::new(std::cell::RefCell::new(vec![1, 2, 3]));
        let ptr = owner.borrow_mut().as_mut_ptr();
        let s = unsafe { ByteStore::external(ptr, 3, owner.clone()) }.with_max_len(8);
        assert!(s.is_external());
        s.bytes_mut()[0] = 42;
        assert_eq!(unsafe { *ptr }, 42);
        s.resize(4).unwrap();
        assert!(!s.is_external());
        s.bytes_mut()[1] = 0;
        assert_eq!(unsafe { *ptr.add(1) }, 2, "a resized external store is a heap copy");
        assert_eq!(&*s.bytes(), [42, 0, 3, 0]);
    }

    #[test]
    fn range_copies() {
        let s = ByteStore::zeroed(4);
        assert!(s.write_at(2, &[1, 2]));
        assert!(!s.write_at(3, &[1, 2]));
        let mut out = [0u8; 2];
        assert!(s.read_at(2, &mut out));
        assert_eq!(out, [1, 2]);
        assert!(!s.read_at(usize::MAX, &mut out));
    }

    #[test]
    fn pressure_accounting() {
        after_gc();
        assert!(!gc_pressure());
        let big = ByteStore::zeroed(EXTERNAL_GC_MIN + 1);
        assert!(gc_pressure());
        after_gc();
        assert!(!gc_pressure());
        drop(big);
        after_gc();
    }
}
