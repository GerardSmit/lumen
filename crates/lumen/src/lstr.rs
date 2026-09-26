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
//! per-thread registry with the list of its views for this. Compaction moves view bytes, so it
//! runs only where nothing can hold a `&str` borrowed from one: between host tasks.

use std::alloc::{alloc, dealloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

struct Header {
    strong: Cell<usize>,
    len: Cell<u32>,
    cap: Cell<u32>,
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
/// The capacity bits of `cap`.
const CAP_MASK: u32 = ROOT - 1;
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

thread_local! {
    static ROOTS: std::cell::RefCell<crate::fasthash::FastMap<usize, Root>> =
        std::cell::RefCell::new(Default::default());
}

fn layout(cap: u32) -> Layout {
    Layout::from_size_align(HDR + cap as usize, std::mem::align_of::<Header>())
        .expect("string too large")
}

impl LStr {
    /// Allocate with `cap` bytes of capacity, seeding `content` (must fit).
    fn alloc(content: &str, cap: u32) -> LStr {
        debug_assert!(content.len() <= cap as usize);
        debug_assert!(cap & !CAP_MASK == 0, "capacity claims a flag bit");
        unsafe {
            let p = alloc(layout(cap)) as *mut Header;
            let p = NonNull::new(p).expect("allocation failed");
            // The hint holds for `content`; constructors that append more bytes afterwards
            // re-AND it with the extra bytes' ASCII-ness (see concat2/concat_grown).
            let hint = if content.is_ascii() { ASCII_HINT } else { 0 };
            p.as_ptr().write(Header {
                strong: Cell::new(1),
                len: Cell::new(content.len() as u32),
                cap: Cell::new(cap | hint),
            });
            let data = (p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(content.as_ptr(), data, content.len());
            LStr { p }
        }
    }

    /// Whether the content is KNOWN all-ASCII (see [`ASCII_HINT`]).
    #[inline]
    pub(crate) fn ascii_hint(&self) -> bool {
        self.hdr().cap.get() & ASCII_HINT != 0
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
        unsafe {
            if self.hdr().cap.get() & VIEW != 0 {
                (*(self.p.as_ptr() as *const ViewHdr)).data()
            } else {
                (self.p.as_ptr() as *const u8).add(HDR)
            }
        }
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
        } else {
            self.p
        };
        let rh = unsafe { root.as_ref() };
        if (rh.len.get() as usize) < VIEW_ROOT_MIN {
            return LStr::from(piece);
        }
        let ascii = self.ascii_hint() || piece.is_ascii();
        rh.strong.set(rh.strong.get() + 1);
        rh.cap.set(rh.cap.get() | ROOT);
        unsafe {
            let v = alloc(Layout::new::<ViewHdr>()) as *mut ViewHdr;
            let v = NonNull::new(v).expect("allocation failed");
            let slot = ROOTS.with(|r| {
                let mut r = r.borrow_mut();
                let e = r
                    .entry(root.as_ptr() as usize)
                    .or_insert_with(|| Root { views: Vec::new(), bytes: 0 });
                e.views.push(v);
                e.bytes += piece.len();
                (e.views.len() - 1) as u32
            });
            v.as_ptr().write(ViewHdr {
                h: Header {
                    strong: Cell::new(1),
                    len: Cell::new(piece.len() as u32),
                    cap: Cell::new(VIEW | if ascii { ASCII_HINT } else { 0 }),
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

    /// Free a view whose last reference is going: unregister it and release its root.
    #[cold]
    unsafe fn drop_view(&mut self) {
        let v = self.p.as_ptr() as *mut ViewHdr;
        let (root, slot, len) = ((*v).root, (*v).slot as usize, (*v).h.len.get() as usize);
        ROOTS.with(|r| {
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
        self.hdr().strong.get()
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
        let h = self.hdr();
        h.strong.set(h.strong.get() + 1);
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
            if cap & VIEW != 0 {
                unsafe { self.drop_view() };
                return;
            }
            unsafe { dealloc(self.p.as_ptr() as *mut u8, layout(cap & CAP_MASK)) };
        } else {
            h.strong.set(s - 1);
        }
    }
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
        LStr::ptr_eq(self, other) || self.as_str() == other.as_str()
    }
}
impl Eq for LStr {}

impl PartialEq<str> for LStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
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
    let keys: Vec<usize> = ROOTS.with(|r| {
        r.borrow()
            .iter()
            .filter(|(&k, e)| unsafe {
                let h = &*(k as *const Header);
                h.strong.get() == e.views.len() && e.bytes < h.len.get() as usize / 2
            })
            .map(|(&k, _)| k)
            .collect()
    });
    let mut freed = 0;
    for k in keys {
        let Some(mut e) = ROOTS.with(|r| r.borrow_mut().remove(&k)) else {
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
            nh.cap.set(total as u32 | ((*old).cap.get() & ASCII_HINT) | ROOT);
            nh.strong.set(e.views.len());
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
            e.bytes = e.views.iter().map(|v| (*v.as_ptr()).h.len.get() as usize).sum();
            ROOTS.with(|r| r.borrow_mut().insert(nk, e));
        }
    }
    freed
}

/// `(roots, views, root bytes)` of the view registry (memory accounting and tests).
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
pub fn view_stats() -> (usize, usize, usize) {
    ROOTS.with(|r| {
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

    #[test]
    fn views_share_and_compact() {
        let text: String = (0..200).map(|i| format!("line number {i:04} of the long list é\n")).collect();
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
}
