//! The engine string: a thin refcounted UTF-8 buffer with spare capacity.
//!
//! `Value::Str` used to hold `Rc<str>` — immutable and exactly sized, which makes an append loop
//! (`s += x`, astring's `this.output += e`) inherently O(n²): every step materializes a fresh
//! allocation of the whole accumulated string. `LStr` is the classic engine fix (QuickJS's
//! JSString): `{strong, len, cap}` header + bytes, one thin pointer. When a string is *uniquely
//! referenced* an append writes in place (amortized by capacity doubling); shared strings copy
//! first, exactly like `Rc::make_mut`.
//!
//! The payload is a single 8-byte pointer to the header. Logical content is always `len` bytes of
//! valid UTF-8 (lone surrogates smuggled, as before — see [`crate::jstr`]); capacity beyond `len`
//! is invisible to every reader because `Deref` slices to `len`.
//!
//! Like `Rc`, `LStr` is neither `Send` nor `Sync` (non-atomic count; the engine is one thread
//! per realm).
//!
//! ## Views
//!
//! A piece of a long string ([`LStr::sub`]: `slice`, `substring`, `trim`, `split`, regex
//! captures, ...) is a *view*: a [`ViewHdr`] (the same header, then the offset of its bytes in
//! the string that owns them, the *root*, and a counted reference to it) instead of a copy. A view of a
//! view points at the root directly, so there are never chains. Views never own spare
//! capacity (so they are never appended to in place) and are at least [`VIEW_MIN`] bytes long.
//!
//! A root whose only references are its views, and whose views cover little of it, is
//! *compacted* ([`compact_views`]): the bytes the views use are copied into one tight buffer,
//! the views are repointed at it, and the root is freed. Every root with views is in a
//! driver-owned registry with the list of its views for this (coroutine threads use their
//! driver’s registry under the exclusive handoff). Compaction moves view bytes, so it
//! runs only where nothing can hold a `&str` borrowed from one: between host tasks.

use std::alloc::{alloc, dealloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

#[repr(C, align(8))]
struct Header {
    /// Saturates at `u32::MAX` (then the string is immortal); see [`STRONG_STICKY`].
    strong: Cell<u32>,
    len: Cell<u32>,
    cap: Cell<u32>,
    /// Concatenation nodes only: [`CONS_FIRST_LOW`] | [`CONS_LAST_HIGH`] | [`CONS_ASCII`].
    spare: Cell<u32>,
    // `cap` bytes of UTF-8 follow.
}

/// See the module docs. `repr(transparent)`-thin: one pointer.
pub struct LStr {
    p: NonNull<Header>,
}

const HDR: usize = std::mem::size_of::<Header>();
/// Offsets of the header's `u32` byte length and `u32` capacity (with [`ASCII_HINT`]), for the
/// optimizing tier's inline `.length` (see `bytecode::jit::layout`).
pub(crate) const LSTR_LEN_OFFSET: usize = std::mem::offset_of!(Header, len);
pub(crate) const LSTR_CAP_OFFSET: usize = std::mem::offset_of!(Header, cap);
/// Offset of the header's strong count (the JIT's inline retain / release of a string that
/// stays alive).
pub(crate) const LSTR_STRONG_OFFSET: usize = std::mem::offset_of!(Header, strong);
/// A strong count at this value never changes again: a string with 2^32 references leaks
/// rather than wrapping into a premature free.
pub(crate) const STRONG_STICKY: u32 = u32::MAX;
/// Offset of the content bytes from the header (the JIT's inline `charCodeAt`).
pub(crate) const LSTR_DATA_OFFSET: usize = HDR;

/// Top bit of `cap`: the content is KNOWN all-ASCII (byte index == UTF-16 unit index, and every
/// byte IS its unit). Purely a hint — never set for non-ASCII content, may be clear for ASCII
/// content. Maintained by every constructor/mutator; capacity readers mask it off.
pub(crate) const ASCII_HINT: u32 = 1 << 31;
/// Bit 30 of `cap`: the header is a [`ViewHdr`] (its bytes are elsewhere; capacity 0).
pub(crate) const VIEW: u32 = 1 << 30;
/// Bit 29 of `cap`: the string has been the root of views (it may be in the registry).
const ROOT: u32 = 1 << 29;
/// Bit 28 of `cap`: the header is a [`ConsHdr`], a lazy concatenation (capacity 0, never the
/// [`ASCII_HINT`] bit: the optimizing tier reads that bit to decide it may read bytes inline).
const CONS: u32 = 1 << 28;
/// The capacity bits of `cap`.
const CAP_MASK: u32 = CONS - 1;
/// The shortest result `a + b` is left as a lazy concatenation (a [`ConsHdr`]) instead of being
/// copied. The optimizing tier's inline one-byte compare relies on this exceeding one byte.
pub const CONS_MIN: usize = 256;
const CONS_FIRST_LOW: u32 = 1;
const CONS_LAST_HIGH: u32 = 2;
const CONS_ASCII: u32 = 4;
/// Offsets of a view's root pointer and byte offset (the JIT's inline `charCodeAt` on a view).
pub(crate) const VIEW_ROOT_OFFSET: usize = std::mem::offset_of!(ViewHdr, root);
pub(crate) const VIEW_OFF_OFFSET: usize = std::mem::offset_of!(ViewHdr, off);
/// The shortest piece made a view: below it a copy (16-byte header + the bytes) is no bigger
/// than a view (32-byte header + its registry entry). The JIT's inline one-byte string compare
/// relies on views never being that short.
pub const VIEW_MIN: usize = 32;
/// The shortest root views are taken of (pieces of short strings are copied).
const VIEW_ROOT_MIN: usize = 256;

/// The header of a view (see the module docs).
#[repr(C)]
struct ViewHdr {
    h: Header,
    /// The root, one counted reference.
    root: NonNull<Header>,
    /// Where the bytes start in the root's.
    off: u32,
    /// Index in the root's registry entry.
    slot: u32,
}

impl ViewHdr {
    #[inline]
    unsafe fn data(&self) -> *const u8 {
        (self.root.as_ptr() as *const u8).add(HDR + self.off as usize)
    }
}

/// A lazy concatenation: `left + right` until something reads the bytes, then a counted
/// reference to the one flat buffer holding them (`left`/`right` released). `h.len` is the
/// total byte length from the start. Chains are flattened and dropped iteratively, so a
/// million-deep `s = s + x` history costs no stack.
#[repr(C)]
struct ConsHdr {
    h: Header,
    left: Cell<Option<NonNull<Header>>>,
    right: Cell<Option<NonNull<Header>>>,
    flat: Cell<Option<NonNull<Header>>>,
}

/// The registry entry of a root with views.
struct Root {
    views: Vec<NonNull<ViewHdr>>,
    /// Sum of the views' lengths (overlaps counted twice).
    bytes: usize,
}

/// Views are on unless `LUMEN_NO_STR_VIEWS` is set (A/B measurement).
fn views_on() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
    static ON: AtomicU8 = AtomicU8::new(0);
    match ON.load(Relaxed) {
        0 => {
            let on = std::env::var_os("LUMEN_NO_STR_VIEWS").is_none();
            ON.store(if on { 1 } else { 2 }, Relaxed);
            on
        }
        v => v == 1,
    }
}

/// Driver-owned view registry shared only under coroutine's exclusive ping-pong handoff.
/// Views can be created on a pooled coroutine thread and dropped/compacted by its driver.
#[derive(Default)]
pub(crate) struct ViewRegistry {
    roots: std::cell::RefCell<crate::fasthash::FastMap<usize, Root>>,
    native_stacks: Cell<usize>,
    generation: Cell<u32>,
    #[cfg_attr(not(feature = "embed"), allow(dead_code))]
    scanned: Cell<u32>,
    #[cfg_attr(not(feature = "embed"), allow(dead_code))]
    skipped: Cell<u32>,
}
impl ViewRegistry {
    pub(crate) fn start_native_stack(&self) {
        self.native_stacks.set(self.native_stacks.get() + 1);
    }
    pub(crate) fn finish_native_stack(&self) {
        self.native_stacks.set(self.native_stacks.get() - 1);
    }
}
#[cfg(feature = "embed")]
pub(crate) fn views_can_compact() -> bool {
    ROOTS.with(|state| state.borrow().native_stacks.get() == 0)
}
// Like value::GcState, access is exclusive because the driver parks while a coroutine runs.
unsafe impl Send for ViewRegistry {}
unsafe impl Sync for ViewRegistry {}
thread_local! {
    static ROOTS: std::cell::RefCell<std::sync::Arc<ViewRegistry>> =
        std::cell::RefCell::new(std::sync::Arc::new(ViewRegistry::default()));
}
pub(crate) fn view_registry_handle() -> std::sync::Arc<ViewRegistry> {
    ROOTS.with(|state| state.borrow().clone())
}
pub(crate) fn enter_view_registry(
    state: std::sync::Arc<ViewRegistry>,
) -> std::sync::Arc<ViewRegistry> {
    ROOTS.with(|current| std::mem::replace(&mut *current.borrow_mut(), state))
}
fn with_roots<R>(
    f: impl FnOnce(&std::cell::RefCell<crate::fasthash::FastMap<usize, Root>>) -> R,
) -> R {
    ROOTS.with(|state| f(&state.borrow().roots))
}

fn bump_generation() {
    ROOTS.with(|state| {
        let s = state.borrow();
        s.generation.set(s.generation.get().wrapping_add(1));
    });
}

#[cfg_attr(not(feature = "embed"), allow(dead_code))]
const RESCAN_EVERY: u32 = 32;

/// Whether a compaction scan is worth its cost: the registry changed since the last one, or
/// enough calls passed that a root may have become eligible through plain string drops.
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
pub(crate) fn scan_due() -> bool {
    ROOTS.with(|state| {
        let s = state.borrow();
        if s.roots.borrow().is_empty() {
            return false;
        }
        let skipped = s.skipped.get();
        if s.generation.get() == s.scanned.get() && skipped < RESCAN_EVERY {
            s.skipped.set(skipped + 1);
            return false;
        }
        s.scanned.set(s.generation.get());
        s.skipped.set(0);
        true
    })
}

fn layout(cap: u32) -> Layout {
    Layout::from_size_align(HDR + cap as usize, std::mem::align_of::<Header>())
        .expect("string too large")
}

/// `bytes` as text when they are valid UTF-8 the engine holds as is. A 0xF4 byte may start a
/// character it spells as a surrogate pair (see [`lumen_common::smuggle`]), so those go the
/// long way round.
pub(crate) fn plain_utf8(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0xF4) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

impl LStr {
    /// Allocate with `cap` bytes of capacity, seeding `content` (must fit).
    fn alloc(content: &str, cap: u32) -> LStr {
        debug_assert!(content.len() <= cap as usize);
        debug_assert!(cap & !CAP_MASK == 0, "capacity claims a flag bit");
        unsafe {
            let _mem = crate::memstats::enter_refining(crate::memstats::Cat::Strings);
            let p = alloc(layout(cap)) as *mut Header;
            let p = NonNull::new(p).expect("allocation failed");
            // The hint holds for `content`; constructors that append more bytes afterwards
            // re-AND it with the extra bytes' ASCII-ness (see concat2/concat_grown).
            let hint = if content.is_ascii() { ASCII_HINT } else { 0 };
            p.as_ptr().write(Header {
                strong: Cell::new(1),
                len: Cell::new(content.len() as u32),
                cap: Cell::new(cap | hint),
                spare: Cell::new(0),
            });
            let data = (p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(content.as_ptr(), data, content.len());
            LStr { p }
        }
    }

    /// A string filled straight from `read` (the `read(2)` shape: bytes written, 0 at the end),
    /// so the bytes exist once, in the string. `size_hint` is the expected length; a source that
    /// turns out longer grows the buffer. Content that is not valid UTF-8 (or holds characters
    /// the engine spells as surrogate pairs) is decoded lossily through a temporary copy.
    pub fn from_utf8_reader<E>(
        size_hint: usize,
        mut read: impl FnMut(&mut [u8]) -> Result<usize, E>,
    ) -> Result<LStr, E> {
        let cap = size_hint.saturating_add(1);
        assert!(cap <= CAP_MASK as usize, "string too large");
        let mut s = unsafe {
            let _mem = crate::memstats::enter_refining(crate::memstats::Cat::Strings);
            let p = std::alloc::alloc_zeroed(layout(cap as u32)) as *mut Header;
            let p = NonNull::new(p).expect("allocation failed");
            p.as_ptr().write(Header {
                strong: Cell::new(1),
                len: Cell::new(0),
                cap: Cell::new(cap as u32),
                spare: Cell::new(0),
            });
            LStr { p }
        };
        let mut len = 0usize;
        loop {
            let cap = (s.hdr().cap.get() & CAP_MASK) as usize;
            if len == cap {
                s.grow_zeroed(cap);
            }
            let cap = (s.hdr().cap.get() & CAP_MASK) as usize;
            let buf = unsafe { std::slice::from_raw_parts_mut(s.data_mut().add(len), cap - len) };
            let n = read(buf)?;
            if n == 0 {
                break;
            }
            len += n;
        }
        Ok(s.finish_utf8(len))
    }

    /// Double the capacity of a flag-free, uniquely owned string; the new bytes are zero.
    fn grow_zeroed(&mut self, cap: usize) {
        let grown = (cap * 2).min(CAP_MASK as usize);
        assert!(grown > cap, "string too large");
        unsafe {
            let p = std::alloc::realloc(self.p.as_ptr() as *mut u8, layout(cap as u32), HDR + grown);
            self.p = NonNull::new(p as *mut Header).expect("allocation failed");
            self.hdr().cap.set(grown as u32);
            std::ptr::write_bytes(self.data_mut().add(cap), 0, grown - cap);
        }
    }

    #[inline]
    fn data_mut(&mut self) -> *mut u8 {
        unsafe { (self.p.as_ptr() as *mut u8).add(HDR) }
    }

    fn finish_utf8(self, len: usize) -> LStr {
        let bytes = unsafe { std::slice::from_raw_parts((self.p.as_ptr() as *const u8).add(HDR), len) };
        if let Some(text) = plain_utf8(bytes) {
            self.hdr().len.set(len as u32);
            if text.is_ascii() {
                self.mark_ascii();
            }
            return self;
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        LStr::from(lumen_common::smuggle::utf16_text_owned(text))
    }

    /// Whether the content is KNOWN all-ASCII (see [`ASCII_HINT`]).
    #[inline]
    pub(crate) fn ascii_hint(&self) -> bool {
        let h = self.hdr();
        h.cap.get() & ASCII_HINT != 0 || h.cap.get() & CONS != 0 && h.spare.get() & CONS_ASCII != 0
    }

    /// Record content found to be all-ASCII (mutators clear the hint again as needed).
    pub(crate) fn mark_ascii(&self) {
        let h = self.hdr();
        if h.cap.get() & CONS != 0 {
            h.spare.set(h.spare.get() | CONS_ASCII);
        } else {
            h.cap.set(h.cap.get() | ASCII_HINT);
        }
    }

    #[inline]
    fn and_ascii(&self, extra_is_ascii: bool) {
        if !extra_is_ascii {
            let h = self.hdr();
            h.cap.set(h.cap.get() & !ASCII_HINT);
        }
    }

    /// The two halves concatenated (a single copy of each into the result).
    pub fn concat2(a: &str, b: &str) -> LStr {
        let total = a.len() + b.len();
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(a.is_ascii() && b.is_ascii());
        s
    }

    /// The parts concatenated in one allocation (plain byte concatenation: the caller has
    /// ruled out surrogate halves meeting at a seam).
    pub fn concat_n(parts: &[&str]) -> LStr {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        let mut at = 0;
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            for p in parts {
                std::ptr::copy_nonoverlapping(p.as_ptr(), data.add(at), p.len());
                at += p.len();
            }
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(parts.iter().all(|p| p.is_ascii()));
        s
    }

    #[inline]
    fn hdr(&self) -> &Header {
        unsafe { self.p.as_ref() }
    }

    #[inline]
    fn data(&self) -> *const u8 {
        let cap = self.hdr().cap.get();
        if cap & (VIEW | CONS) == 0 {
            return unsafe { (self.p.as_ptr() as *const u8).add(HDR) };
        }
        self.data_indirect(cap)
    }

    #[inline(never)]
    fn data_indirect(&self, cap: u32) -> *const u8 {
        unsafe {
            if cap & VIEW != 0 {
                (*(self.p.as_ptr() as *const ViewHdr)).data()
            } else {
                (self.flat_buffer().as_ptr() as *const u8).add(HDR)
            }
        }
    }

    #[inline]
    fn is_cons(&self) -> bool {
        self.hdr().cap.get() & CONS != 0
    }

    /// The flat buffer of a concatenation node, building it on the first read.
    fn flat_buffer(&self) -> NonNull<Header> {
        debug_assert!(self.is_cons());
        let c = unsafe { &*(self.p.as_ptr() as *const ConsHdr) };
        if let Some(f) = c.flat.get() {
            return f;
        }
        let total = self.hdr().len.get() as usize;
        let out = LStr::alloc("", total as u32);
        unsafe {
            let dst = (out.p.as_ptr() as *mut u8).add(HDR);
            let mut at = 0;
            let mut stack = vec![self.p];
            while let Some(n) = stack.pop() {
                let nh = n.as_ref();
                let leaf = if nh.cap.get() & CONS != 0 {
                    let nc = &*(n.as_ptr() as *const ConsHdr);
                    match nc.flat.get() {
                        Some(f) => f,
                        None => {
                            stack.push(nc.right.get().expect("unflattened node"));
                            stack.push(nc.left.get().expect("unflattened node"));
                            continue;
                        }
                    }
                } else {
                    n
                };
                let piece = std::mem::ManuallyDrop::new(LStr { p: leaf });
                let bytes = piece.as_str().as_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(at), bytes.len());
                at += bytes.len();
            }
            debug_assert_eq!(at, total);
            out.hdr().len.set(total as u32);
            if self.hdr().spare.get() & CONS_ASCII != 0 {
                out.mark_ascii();
            }
            let (l, r) = (c.left.take(), c.right.take());
            let flat = out.p;
            std::mem::forget(out);
            c.flat.set(Some(flat));
            release_all([l, r]);
            flat
        }
    }

    /// `a + b` without copying when the result is long: a lazy concatenation, flattened on
    /// first read. `None` when the result is short, one side is empty, or a smuggled surrogate
    /// pair would form across the seam (the caller concatenates the flat bytes, with the join
    /// fix-up).
    pub fn cons(a: &LStr, b: &LStr) -> Option<LStr> {
        let total = a.len().checked_add(b.len())?;
        if total < CONS_MIN || a.is_empty() || b.is_empty() || a.last_high() && b.first_low() {
            return None;
        }
        let total = u32::try_from(total).ok().filter(|&t| t <= CAP_MASK)?;
        let mut spare = 0;
        if a.first_low() {
            spare |= CONS_FIRST_LOW;
        }
        if b.last_high() {
            spare |= CONS_LAST_HIGH;
        }
        if a.ascii_hint() && b.ascii_hint() {
            spare |= CONS_ASCII;
        }
        unsafe {
            let _mem = crate::memstats::enter_refining(crate::memstats::Cat::Strings);
            let p = alloc(Layout::new::<ConsHdr>()) as *mut ConsHdr;
            let p = NonNull::new(p).expect("allocation failed");
            bump(&a.hdr().strong);
            bump(&b.hdr().strong);
            p.as_ptr().write(ConsHdr {
                h: Header {
                    strong: Cell::new(1),
                    len: Cell::new(total),
                    cap: Cell::new(CONS),
                    spare: Cell::new(spare),
                },
                left: Cell::new(Some(a.p)),
                right: Cell::new(Some(b.p)),
                flat: Cell::new(None),
            });
            Some(LStr { p: p.cast() })
        }
    }

    /// The byte length (no flattening).
    #[inline]
    pub fn len(&self) -> usize {
        self.hdr().len.get() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the first character is a smuggled low surrogate.
    fn first_low(&self) -> bool {
        if self.is_cons() {
            return self.hdr().spare.get() & CONS_FIRST_LOW != 0;
        }
        self.as_str()
            .chars()
            .next()
            .is_some_and(|c| crate::jstr::smuggled_low(c).is_some())
    }

    /// Whether the last character is a smuggled high surrogate.
    fn last_high(&self) -> bool {
        if self.is_cons() {
            return self.hdr().spare.get() & CONS_LAST_HIGH != 0;
        }
        self.as_str()
            .chars()
            .next_back()
            .is_some_and(|c| crate::jstr::smuggled_high(c).is_some())
    }

    /// All of `parts` concatenated, `None` when a smuggled surrogate pair would form across a
    /// seam. A long first part (a growing accumulator in `s = s + a + b`) stays a lazy
    /// concatenation with the rest; everything else is one allocation.
    pub fn concat_all(parts: &[&LStr]) -> Option<LStr> {
        if parts.windows(2).any(|w| w[0].last_high() && w[1].first_low()) {
            return None;
        }
        if let [first, rest @ ..] = parts {
            if first.len() >= CONS_MIN && !rest.is_empty() {
                let tail = match rest {
                    [one] => (*one).clone(),
                    _ => LStr::concat_lstrs(rest),
                };
                if let Some(c) = LStr::cons(first, &tail) {
                    return Some(c);
                }
            }
        }
        Some(LStr::concat_lstrs(parts))
    }

    fn concat_lstrs(parts: &[&LStr]) -> LStr {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        let mut at = 0;
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            for p in parts {
                let p = p.as_str();
                std::ptr::copy_nonoverlapping(p.as_ptr(), data.add(at), p.len());
                at += p.len();
            }
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(parts.iter().all(|p| p.ascii_hint() || p.is_ascii()));
        s
    }

    /// `a + b` (ToString'd operands), a lazy concatenation when long.
    pub fn join(a: &LStr, b: &LStr) -> LStr {
        if let Some(c) = LStr::cons(a, b) {
            return c;
        }
        if a.last_high() && b.first_low() {
            return LStr::from(crate::jstr::concat(a, b));
        }
        LStr::concat2(a, b)
    }

    /// `piece` (a subslice of `self`) as a string: a view of `self`'s root when it is long
    /// enough to be worth one (see the module docs), else a copy (or `self` for all of it).
    pub fn sub(&self, piece: &str) -> LStr {
        let whole = self.as_str();
        let at = (piece.as_ptr() as usize).wrapping_sub(whole.as_ptr() as usize);
        if at == 0 && piece.len() == whole.len() {
            return self.clone();
        }
        if piece.len() < VIEW_MIN || !views_on() {
            return LStr::from(piece);
        }
        if at > whole.len() || at + piece.len() > whole.len() {
            debug_assert!(false, "LStr::sub of a foreign slice");
            return LStr::from(piece);
        }
        let root = if self.is_view() {
            unsafe { (*(self.p.as_ptr() as *const ViewHdr)).root }
        } else if self.is_cons() {
            self.flat_buffer()
        } else {
            self.p
        };
        let rh = unsafe { root.as_ref() };
        if (rh.len.get() as usize) < VIEW_ROOT_MIN {
            return LStr::from(piece);
        }
        let ascii = self.ascii_hint() || piece.is_ascii();
        bump(&rh.strong);
        rh.cap.set(rh.cap.get() | ROOT);
        unsafe {
            let _mem = crate::memstats::enter_refining(crate::memstats::Cat::Strings);
            let v = alloc(Layout::new::<ViewHdr>()) as *mut ViewHdr;
            let v = NonNull::new(v).expect("allocation failed");
            let slot = with_roots(|r| {
                bump_generation();
                let mut r = r.borrow_mut();
                let e = r.entry(root.as_ptr() as usize).or_insert_with(|| Root {
                    views: Vec::new(),
                    bytes: 0,
                });
                e.views.push(v);
                e.bytes += piece.len();
                (e.views.len() - 1) as u32
            });
            v.as_ptr().write(ViewHdr {
                h: Header {
                    strong: Cell::new(1),
                    len: Cell::new(piece.len() as u32),
                    cap: Cell::new(VIEW | if ascii { ASCII_HINT } else { 0 }),
                    spare: Cell::new(0),
                },
                root,
                off: (piece.as_ptr() as usize - (root.as_ptr() as usize + HDR)) as u32,
                slot,
            });
            LStr { p: v.cast() }
        }
    }

    /// Whether this string is a view (see the module docs).
    #[inline]
    pub fn is_view(&self) -> bool {
        self.hdr().cap.get() & VIEW != 0
    }

    /// `prefix + self` as a view of `self`'s root, when `self` is a view whose root holds
    /// exactly `prefix` right before it. Lets a chain of prefixed strings ("bound bound … f")
    /// share one buffer instead of copying the whole string per link.
    pub fn view_extend_front(&self, prefix: &str) -> Option<LStr> {
        if !self.is_view() {
            return None;
        }
        let (root, off) = unsafe {
            let v = &*(self.p.as_ptr() as *const ViewHdr);
            (
                std::mem::ManuallyDrop::new(LStr { p: v.root }),
                v.off as usize,
            )
        };
        let start = off.checked_sub(prefix.len())?;
        let whole = root.as_str();
        if whole.as_bytes().get(start..off)? != prefix.as_bytes() {
            return None;
        }
        let piece = whole.get(start..off + self.len())?;
        Some(root.sub(piece))
    }

    #[cold]
    unsafe fn drop_indirect(&mut self, cap: u32) {
        if cap & VIEW != 0 {
            self.drop_view();
        } else {
            release_all([Some(self.p)]);
        }
    }

    /// Free a view whose last reference is going: unregister it and release its root.
    #[cold]
    unsafe fn drop_view(&mut self) {
        let v = self.p.as_ptr() as *mut ViewHdr;
        let (root, slot, len) = ((*v).root, (*v).slot as usize, (*v).h.len.get() as usize);
        bump_generation();
        with_roots(|r| {
            let mut r = r.borrow_mut();
            let key = root.as_ptr() as usize;
            if let Some(e) = r.get_mut(&key) {
                e.views.swap_remove(slot);
                if let Some(moved) = e.views.get(slot) {
                    (*moved.as_ptr()).slot = slot as u32;
                }
                e.bytes = e.bytes.saturating_sub(len);
                if e.views.is_empty() {
                    r.remove(&key);
                } else if e.views.capacity() > 64 && e.views.len() < e.views.capacity() / 4 {
                    // A split's list of 100k views must not stay that big when 1% survive.
                    e.views.shrink_to(e.views.len() * 2);
                }
            }
        });
        dealloc(v as *mut u8, Layout::new::<ViewHdr>());
        drop(LStr { p: root });
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe {
            let bytes = std::slice::from_raw_parts(self.data(), self.hdr().len.get() as usize);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    /// Pointer identity (cache keys — same contract as `Rc::as_ptr`).
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.p.as_ptr() as *const u8
    }

    #[inline]
    pub fn ptr_eq(a: &LStr, b: &LStr) -> bool {
        a.p == b.p
    }

    #[inline]
    pub fn strong_count(&self) -> usize {
        self.hdr().strong.get() as usize
    }

    /// Append in place when this is the ONLY reference and capacity suffices. Returns false
    /// (without modifying anything) otherwise — the caller copies. The unique-owner requirement
    /// is what makes the mutation invisible: no other handle can observe the content, and the
    /// caller must not hold a `&str` borrow of `self` across the call (enforced by `&mut self`).
    pub fn append_in_place(&mut self, x: &str) -> bool {
        let h = self.hdr();
        if h.strong.get() != 1 {
            return false;
        }
        let len = h.len.get() as usize;
        if len + x.len() > (h.cap.get() & CAP_MASK) as usize {
            return false;
        }
        unsafe {
            let data = (self.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(len), x.len());
        }
        h.len.set((len + x.len()) as u32);
        self.and_ascii(x.is_ascii());
        true
    }

    /// `self + x` with growth capacity: used by the fused append ops when in-place didn't apply.
    /// Doubles (at least) so a rebuilt accumulator amortizes the next appends.
    pub fn concat_grown(&self, x: &str) -> LStr {
        let need = self.as_str().len() + x.len();
        let cap = u32::try_from((need * 2).max(32))
            .unwrap_or(CAP_MASK)
            .min(CAP_MASK); // the top bits are flags, never capacity
        let s = LStr::alloc(self.as_str(), cap.max(need as u32));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(self.as_str().len()), x.len());
        }
        s.hdr().len.set(need as u32);
        s.and_ascii(x.is_ascii());
        s
    }
}

impl Clone for LStr {
    #[inline]
    fn clone(&self) -> LStr {
        bump(&self.hdr().strong);
        LStr { p: self.p }
    }
}

impl Drop for LStr {
    #[inline]
    fn drop(&mut self) {
        let h = self.hdr();
        let s = h.strong.get();
        if s == 1 {
            let cap = h.cap.get();
            if cap & (VIEW | CONS) != 0 {
                unsafe { self.drop_indirect(cap) };
                return;
            }
            unsafe { dealloc(self.p.as_ptr() as *mut u8, layout(cap & CAP_MASK)) };
        } else if s != STRONG_STICKY {
            h.strong.set(s - 1);
        }
    }
}

/// Release one reference to each of `nodes`, freeing the ones that were the last and, for a
/// concatenation node, the references it held — iteratively, however deep the chain.
unsafe fn release_all<const N: usize>(nodes: [Option<NonNull<Header>>; N]) {
    let mut stack: Vec<NonNull<Header>> = nodes.into_iter().flatten().collect();
    while let Some(n) = stack.pop() {
        let h = n.as_ref();
        let strong = h.strong.get();
        if strong == STRONG_STICKY {
            continue;
        }
        if strong > 1 {
            h.strong.set(strong - 1);
            continue;
        }
        let cap = h.cap.get();
        if cap & CONS != 0 {
            let c = &*(n.as_ptr() as *const ConsHdr);
            stack.extend(c.left.take());
            stack.extend(c.right.take());
            stack.extend(c.flat.take());
            dealloc(n.as_ptr() as *mut u8, Layout::new::<ConsHdr>());
        } else {
            drop(LStr { p: n });
        }
    }
}


#[inline(always)]
fn bump(c: &Cell<u32>) {
    let s = c.get();
    c.set(s + (s != STRONG_STICKY) as u32);
}

impl std::ops::Deref for LStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for LStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for LStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for LStr {
    fn from(s: &str) -> LStr {
        LStr::alloc(s, u32::try_from(s.len()).expect("string too large"))
    }
}

impl From<String> for LStr {
    fn from(s: String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<std::rc::Rc<str>> for LStr {
    fn from(s: std::rc::Rc<str>) -> LStr {
        LStr::from(&*s)
    }
}

impl From<&String> for LStr {
    fn from(s: &String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<char> for LStr {
    fn from(c: char) -> LStr {
        LStr::from(c.encode_utf8(&mut [0u8; 4]) as &str)
    }
}

impl PartialEq for LStr {
    fn eq(&self, other: &LStr) -> bool {
        LStr::ptr_eq(self, other)
            || lumen_common::scan::bytes_equal(self.as_bytes(), other.as_bytes())
    }
}
impl Eq for LStr {}

impl PartialEq<str> for LStr {
    fn eq(&self, other: &str) -> bool {
        lumen_common::scan::bytes_equal(self.as_bytes(), other.as_bytes())
    }
}

impl std::hash::Hash for LStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl std::fmt::Display for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<&LStr> for std::rc::Rc<str> {
    fn from(s: &LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}

impl From<LStr> for std::rc::Rc<str> {
    fn from(s: LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}

/// Compact the roots only views still reference and that the views cover less than half of
/// (see the module docs); returns the bytes freed. The caller guarantees no `&str` borrowed from
/// a view is alive (it runs between host tasks).
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
pub fn compact_views() -> usize {
    let keys: Vec<usize> = with_roots(|r| {
        r.borrow()
            .iter()
            .filter(|(&k, e)| unsafe {
                let h = &*(k as *const Header);
                h.strong.get() as usize == e.views.len() && e.bytes < h.len.get() as usize / 2
            })
            .map(|(&k, _)| k)
            .collect()
    });
    let mut freed = 0;
    for k in keys {
        let Some(mut e) = with_roots(|r| r.borrow_mut().remove(&k)) else {
            continue;
        };
        unsafe {
            let old = k as *mut Header;
            let base = (old as *const u8).add(HDR);
            let off = |v: &NonNull<ViewHdr>| (*v.as_ptr()).off as usize;
            // The used ranges, merged (views overlap: a line and its trimmed middle).
            let mut ranges: Vec<(usize, usize)> = e
                .views
                .iter()
                .map(|v| (off(v), off(v) + (*v.as_ptr()).h.len.get() as usize))
                .collect();
            ranges.sort_unstable();
            // (from, to, offset in the new buffer)
            let mut merged: Vec<(usize, usize, usize)> = Vec::new();
            let mut total = 0;
            for (a, b) in ranges {
                match merged.last_mut() {
                    Some(m) if a <= m.1 => {
                        if b > m.1 {
                            total += b - m.1;
                            m.1 = b;
                        }
                    }
                    _ => {
                        merged.push((a, b, total));
                        total += b - a;
                    }
                }
            }
            let new = LStr::alloc("", total as u32);
            let nd = (new.p.as_ptr() as *mut u8).add(HDR);
            for &(a, b, at) in &merged {
                std::ptr::copy_nonoverlapping(base.add(a), nd.add(at), b - a);
            }
            let nh = new.hdr();
            nh.len.set(total as u32);
            nh.cap
                .set(total as u32 | ((*old).cap.get() & ASCII_HINT) | ROOT);
            nh.strong.set(e.views.len() as u32);
            for v in &e.views {
                let at = off(v);
                let m = merged[merged.partition_point(|m| m.0 <= at) - 1];
                (*v.as_ptr()).off = (m.2 + (at - m.0)) as u32;
                (*v.as_ptr()).root = new.p;
            }
            freed += (*old).len.get() as usize - total;
            dealloc(old as *mut u8, layout((*old).cap.get() & CAP_MASK));
            let nk = new.p.as_ptr() as usize;
            std::mem::forget(new);
            e.views.shrink_to_fit();
            e.bytes = e
                .views
                .iter()
                .map(|v| (*v.as_ptr()).h.len.get() as usize)
                .sum();
            with_roots(|r| r.borrow_mut().insert(nk, e));
        }
    }
    freed
}

/// `(roots, views, root bytes)` of the view registry (memory accounting and tests).
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
pub fn view_stats() -> (usize, usize, usize) {
    with_roots(|r| {
        let r = r.borrow();
        let views = r.values().map(|e| e.views.len()).sum();
        let bytes = r
            .keys()
            .map(|&k| unsafe { (*(k as *const Header)).len.get() as usize })
            .sum();
        (r.len(), views, bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_chunks(data: &[u8], chunk: usize) -> impl FnMut(&mut [u8]) -> Result<usize, ()> + '_ {
        let mut at = 0;
        move |buf| {
            let n = chunk.min(buf.len()).min(data.len() - at);
            buf[..n].copy_from_slice(&data[at..at + n]);
            at += n;
            Ok(n)
        }
    }

    fn from_reader(hint: usize, data: &[u8], chunk: usize) -> LStr {
        LStr::from_utf8_reader(hint, read_chunks(data, chunk)).unwrap()
    }

    #[test]
    fn reader_builds_the_same_string_as_decoding() {
        let ascii = "plain ascii text\nline two";
        let s = from_reader(ascii.len(), ascii.as_bytes(), 7);
        assert_eq!((&*s, s.ascii_hint()), (ascii, true));

        let multi = "h\u{e9}llo \u{4e16}\u{754c} \u{1f600}";
        let s = from_reader(multi.len(), multi.as_bytes(), 3);
        assert_eq!((&*s, s.ascii_hint()), (multi, false));

        let s = from_reader(0, b"", 8);
        assert_eq!((&*s, s.len()), ("", 0));

        let bad = [b'a', 0xF0, 0x9F, b'b', 0xC0, 0x80, 0xFF];
        let s = from_reader(bad.len(), &bad, 2);
        assert_eq!(&*s, String::from_utf8_lossy(&bad));
    }

    #[test]
    fn reader_survives_a_source_that_changes_size() {
        let text = "x\u{e9}".repeat(5000);
        let grown = from_reader(10, text.as_bytes(), 1000);
        assert_eq!(&*grown, text.as_str());
        let shrunk = from_reader(text.len() * 3, text.as_bytes(), 4096);
        assert_eq!(&*shrunk, text.as_str());
        let mut s = LStr::from_utf8_reader(0, read_chunks(text.as_bytes(), 777)).unwrap();
        assert!(s.append_in_place("tail") || { s = s.concat_grown("tail"); true });
        assert!(s.ends_with("tail") && s.len() == text.len() + 4);
    }

    #[test]
    fn reader_respells_characters_above_the_smuggle_base() {
        let high = "a\u{10FFFF}b";
        let s = from_reader(high.len(), high.as_bytes(), 16);
        assert_eq!(&*s, lumen_common::smuggle::utf16_text_owned(high.to_string()));
        assert_ne!(&*s, high);
    }

    #[test]
    fn reader_error_frees_the_partial_string() {
        let r = LStr::from_utf8_reader(8, |_: &mut [u8]| Err::<usize, _>("eio"));
        assert_eq!(r.err(), Some("eio"));
    }

    #[test]
    fn views_share_and_compact() {
        let text: String = (0..200)
            .map(|i| format!("line number {i:04} of the long list é\n"))
            .collect();
        let root = LStr::from(text.as_str());
        let lines: Vec<LStr> = root.split('\n').map(|l| root.sub(l)).collect();
        assert!(lines[3].is_view() && &*lines[3] == "line number 0003 of the long list é");
        // A view of a view is a view of the root.
        let mid = lines[3].sub(&lines[3][2..]);
        assert!(mid.is_view() && &*mid == "ne number 0003 of the long list é");
        // Short pieces are copies; the whole string is the string itself.
        assert!(!lines[3].sub(&lines[3][..4]).is_view());
        assert!(LStr::ptr_eq(&root.sub(&root), &root));
        // An append never writes into the root.
        let mut s = lines[5].clone();
        drop(lines[5].clone());
        assert!(!s.append_in_place("x"));
        s = s.concat_grown("x");
        assert!(s.ends_with("éx") && &*lines[6] == "line number 0006 of the long list é");
        // Only views left, covering little: compaction copies them out and frees the root.
        let keep = [lines[7].clone(), mid.clone(), lines[4].clone()];
        drop(lines);
        drop(root);
        let (_, views, bytes) = view_stats();
        assert_eq!((views, bytes), (3, text.len()));
        let used: usize = keep.iter().map(|k| k.len()).sum();
        assert_eq!(compact_views(), text.len() - used);
        assert_eq!(&*keep[0], "line number 0007 of the long list é");
        assert_eq!(&*keep[1], "ne number 0003 of the long list é");
        assert_eq!(&*keep[2], "line number 0004 of the long list é");
        assert_eq!(view_stats().1, 3);
        drop(keep);
        drop(mid);
        drop(s);
        assert_eq!(view_stats(), (0, 0, 0));
    }

    fn long(c: char, n: usize) -> LStr {
        LStr::from(c.to_string().repeat(n))
    }

    #[test]
    fn long_concatenations_stay_lazy_and_flatten_once() {
        let (a, b) = (long('a', 200), long('b', 100));
        let c = LStr::join(&a, &b);
        assert!(c.is_cons() && c.len() == 300 && c.ascii_hint());
        assert!(c.hdr().cap.get() & ASCII_HINT == 0, "the optimizing tier must not see the hint");
        assert_eq!(a.strong_count(), 2, "operands stay shared with the node");
        let first = c.as_str().as_ptr();
        assert_eq!(&c[198..202], "aabb");
        assert_eq!(c.as_str().as_ptr(), first, "flattened in place, once");
        assert_eq!(a.strong_count(), 1, "flattening releases the operands");
        let short = LStr::join(&long('x', 10), &long('y', 10));
        assert!(!short.is_cons());
    }

    #[test]
    fn a_million_deep_chain_flattens_and_drops_without_recursion() {
        let mut s = long('x', 300);
        let piece = LStr::from("ab");
        for _ in 0..1_000_000 {
            s = LStr::join(&s, &piece);
        }
        assert_eq!(s.len(), 300 + 2_000_000);
        assert!(s.starts_with("xxx") && &s[298..304] == "xxabab" && s.ends_with("abab"));
        let mut t = long('y', 300);
        for _ in 0..1_000_000 {
            t = LStr::join(&piece, &t);
        }
        drop(t);
    }

    #[test]
    fn views_of_a_concatenation_read_its_flat_bytes() {
        let c = LStr::join(&long('a', 200), &long('b', 200));
        let piece = c.sub(&c[190..260]);
        assert!(piece.is_view() && piece.len() == 70);
        drop(c);
        assert_eq!(&piece[8..14], "aabbbb");
    }

    #[test]
    fn smuggled_surrogates_join_across_a_lazy_seam() {
        let high = LStr::from(crate::jstr::from_units(&[0xD83D]));
        let low = LStr::from(crate::jstr::from_units(&[0xDE00]));
        let pad = long('p', 300);
        let left = LStr::join(&pad, &high);
        let right = LStr::join(&low, &pad);
        assert!(left.last_high() && right.first_low());
        assert!(LStr::cons(&left, &right).is_none());
        let joined = LStr::join(&left, &right);
        assert_eq!(crate::jstr::unit_len(&joined), 602);
        assert_eq!(joined.chars().filter(|&c| c == '\u{1F600}').count(), 1);
        let apart = LStr::join(&left, &long('q', 5));
        assert!(apart.is_cons() && apart.last_high() == false);
        let parts = [&pad, &high, &low, &pad];
        assert!(LStr::concat_all(&parts).is_none());
        let ok = LStr::concat_all(&[&pad, &high, &pad]).unwrap();
        assert_eq!(ok.len(), 604);
    }
}
