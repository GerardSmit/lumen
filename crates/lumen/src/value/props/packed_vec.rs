//! Growable packed element buffer with room at both ends.
//!
//! A `Vec<Property>` whose live run may start `head` slots into its allocation, so `shift` and
//! `unshift` move one pointer instead of every element: removing the first element advances
//! the start, and inserting before it reuses the slots freed that way (or opens a fresh run of
//! front slack). Indexed access is unchanged, and the JIT reads `ptr` and the `u32` `len`.
//!
//! A small array's buffer can instead be the element slots trailing its own heap box (`inline`):
//! the vector then owns the elements but not the memory, and outgrowing the slots moves the
//! run to an ordinary heap buffer.
//!
//! Each element is one NaN-boxed word: packed elements are always plain data properties
//! (writable, enumerable, configurable) or holes (`PACK_EMPTY` words), so no descriptor bits
//! are stored. An element that needs anything else moves the array to classic storage.
use crate::value::PackedValue as Elem;
use std::cell::Cell;
use std::mem::ManuallyDrop;

#[repr(C)]
pub(in crate::value) struct PackedVec {
    /// First live element; `head` slots after the allocation start. Null only in the absent
    /// state ([`PackedVec::NONE`]) a sidecar holds while its elements are not packed.
    ptr: *mut Elem,
    len: u32,
    /// Capacity counted from `ptr` (the allocation holds `head + cap`).
    cap: u32,
    /// Uninitialized slots in front of `ptr`.
    head: u32,
    /// No live element is known to be a hole. Cleared by any mutable element access that could
    /// change that; [`PackedVec::all_plain`] re-proves it with a scan.
    plain: Cell<bool>,
    /// Every live element is known to be plain and to hold no heap handle (numbers, booleans,
    /// `undefined`, `null`): copies are a `memcpy` and dropping needs no per-element work.
    /// Implies `plain`; [`PackedVec::all_flat`] re-proves it.
    flat: Cell<bool>,
    /// The slots are the element area of the owning array's heap box (see `DenseStorage`), not
    /// an allocation of this vector: never freed here.
    inline: bool,
}

#[inline]
fn is_plain(p: &Elem) -> bool {
    !p.is_hole()
}

/// Plain, and no handle to release or retain.
#[inline]
fn is_flat(p: &Elem) -> bool {
    !p.is_hole() && !p.needs_drop()
}

/// Drop `len` words at `ptr`, skipping those that hold no handle.
unsafe fn drop_elems(ptr: *mut Elem, len: usize) {
    for k in 0..len {
        let p = ptr.add(k);
        if (*p).needs_drop() {
            std::ptr::drop_in_place(p);
        }
    }
}

pub(in crate::value) const PACKED_PTR: usize = std::mem::offset_of!(PackedVec, ptr);
pub(in crate::value) const PACKED_LEN: usize = std::mem::offset_of!(PackedVec, len);

#[inline]
fn u32_len(n: usize) -> u32 {
    u32::try_from(n).expect("packed elements exceed u32 capacity")
}

impl PackedVec {
    /// The absent state: no packed elements (see `DenseBuffers::packed`).
    pub(in crate::value) const NONE: PackedVec = PackedVec {
        ptr: std::ptr::null_mut(),
        len: 0,
        cap: 0,
        head: 0,
        plain: Cell::new(true),
        flat: Cell::new(true),
        inline: false,
    };

    #[inline(always)]
    pub(in crate::value) fn is_none(&self) -> bool {
        self.ptr.is_null()
    }

    /// Packed storage over `cap` in-box slots at `buf` whose first `len` are initialized.
    ///
    /// # Safety
    /// `buf` must be the element area of the heap box holding the sidecar this vector is
    /// stored in, valid for `cap` elements for as long as that box's object lives.
    #[inline(always)]
    pub(in crate::value) const unsafe fn inline_raw(
        buf: *mut Elem,
        len: usize,
        cap: usize,
    ) -> PackedVec {
        PackedVec {
            ptr: buf,
            len: len as u32,
            cap: cap as u32,
            head: 0,
            plain: Cell::new(len == 0),
            flat: Cell::new(len == 0),
            inline: true,
        }
    }

    pub(in crate::value) fn with_capacity(n: usize) -> PackedVec {
        PackedVec::from(Vec::with_capacity(n))
    }

    /// The buffer as an ordinary vector of capacity at least `min_cap`: the live run is moved
    /// down to the allocation start, or out of in-box slots into a fresh allocation.
    pub(in crate::value) fn into_vec(self, min_cap: usize) -> Vec<Elem> {
        let me = ManuallyDrop::new(self);
        let len = me.len as usize;
        unsafe {
            if me.inline {
                let mut v = Vec::with_capacity(min_cap.max(len));
                std::ptr::copy_nonoverlapping(me.ptr, v.as_mut_ptr(), len);
                v.set_len(len);
                return v;
            }
            let base = me.ptr.sub(me.head as usize);
            if me.head != 0 {
                std::ptr::copy(me.ptr, base, len);
            }
            Vec::from_raw_parts(base, len, (me.cap + me.head) as usize)
        }
    }

    /// Whether every element is a plain data property (see `plain`); a failed proof costs a
    /// scan, a successful one is remembered.
    pub(in crate::value) fn all_plain(&self) -> bool {
        if !self.plain.get() {
            self.plain.set(self.iter().all(is_plain));
        }
        self.plain.get()
    }

    /// Whether every element is flat (see `flat`); memoized like [`Self::all_plain`].
    pub(in crate::value) fn all_flat(&self) -> bool {
        if !self.flat.get() && self.iter().all(is_flat) {
            self.flat.set(true);
            self.plain.set(true);
        }
        self.flat.get()
    }

    /// The remembered proof that every element is plain (no scan: `false` may be stale).
    #[inline]
    pub(in crate::value) fn known_plain(&self) -> bool {
        self.plain.get()
    }

    /// A copy of elements `start..end`, which the caller proved flat: one `memcpy`.
    pub(in crate::value) fn copy_flat(&self, start: usize, end: usize) -> PackedVec {
        let run = &self[start..end];
        let mut v: Vec<Elem> = Vec::with_capacity(run.len());
        // SAFETY: flat words own nothing, so a bitwise copy is a clone.
        unsafe {
            std::ptr::copy_nonoverlapping(run.as_ptr(), v.as_mut_ptr(), run.len());
            v.set_len(run.len());
        }
        let p = PackedVec::from(v);
        p.plain.set(true);
        p.flat.set(true);
        p
    }

    /// Record `p`'s effect on the proofs as it joins the elements.
    #[inline]
    fn note(&self, p: &Elem) {
        let plain = !p.is_hole();
        self.plain.set(self.plain.get() & plain);
        self.flat.set(self.flat.get() & plain & !p.needs_drop());
    }

    /// Element `n` for a value-only write (`set_value` of a non-hole): keeps the `plain` proof
    /// (the value may become a handle: `flat` is dropped).
    #[inline]
    pub(in crate::value) fn get_value_mut(&mut self, n: usize) -> Option<&mut Elem> {
        if n < self.len as usize {
            self.flat.set(false);
            Some(unsafe { &mut *self.ptr.add(n) })
        } else {
            None
        }
    }

    /// Run `f` on the buffer as a `Vec` of capacity at least `min_cap` (for operations that
    /// may reallocate).
    fn with_vec<R>(&mut self, min_cap: usize, f: impl FnOnce(&mut Vec<Elem>) -> R) -> R {
        let (plain, flat) = (self.plain.get(), self.flat.get());
        let taken = std::mem::replace(self, PackedVec::default());
        let mut v = taken.into_vec(min_cap);
        let r = f(&mut v);
        *self = PackedVec::from(v);
        self.plain.set(plain);
        self.flat.set(flat);
        r
    }

    /// Ensure room for `extra` more elements at the back. A buffer with front slack is
    /// compacted first, and then grown so that at least `len` slots stay free: a queue that
    /// shifts and pushes in turn pays for one move per `len` operations. Growth doubles: growing
    /// by half (V8, QuickJS) leaves less slack, but measured a higher peak RSS for large arrays
    /// here — more reallocations, fewer of them extended in place by the system allocator.
    #[inline(never)]
    pub(in crate::value) fn reserve(&mut self, extra: usize) {
        let (len, cap) = (self.len as usize, self.cap as usize);
        if cap - len >= extra {
            return;
        }
        let want = if self.head != 0 {
            extra.max(len)
        } else {
            extra
        };
        let total = cap + self.head as usize;
        if !self.inline && total - len >= want {
            self.with_vec(0, |_| ());
            return;
        }
        let new_cap = (len + want).max(total * 2).max(4);
        self.with_vec(new_cap, |v| v.reserve_exact(new_cap - v.len()));
    }

    #[inline]
    pub(in crate::value) fn push(&mut self, p: Elem) {
        if self.len == self.cap {
            self.reserve(1);
        }
        self.note(&p);
        unsafe { self.ptr.add(self.len as usize).write(p) };
        self.len += 1;
    }

    pub(in crate::value) fn pop(&mut self) -> Option<Elem> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(unsafe { self.ptr.add(self.len as usize).read() })
    }

    /// Remove and return the first element: O(1), the slot becomes front slack.
    pub(in crate::value) fn pop_front(&mut self) -> Option<Elem> {
        if self.len == 0 {
            return None;
        }
        let p = unsafe { self.ptr.read() };
        self.ptr = unsafe { self.ptr.add(1) };
        self.len -= 1;
        self.cap -= 1;
        self.head += 1;
        // Give most of a drained front back once it dwarfs what is live.
        if !self.inline && self.head >= 64 && self.head > 4 * self.len {
            self.with_vec(0, |v| v.shrink_to(v.len() * 2));
        }
        Some(p)
    }

    /// Insert `items` before the first element, in order. Uses front slack when there is
    /// enough, else moves the run once into a buffer with `len` slots of new front slack.
    pub(in crate::value) fn prepend(&mut self, items: impl ExactSizeIterator<Item = Elem>) {
        let n = items.len();
        if n == 0 {
            return;
        }
        if (self.head as usize) < n {
            let slack = n.max(self.len as usize).max(4);
            let mut fresh: Vec<Elem> = Vec::with_capacity(slack + self.cap as usize);
            let base = fresh.as_mut_ptr();
            let old = std::mem::replace(self, PackedVec::default());
            let (len, plain, flat) = (old.len as usize, old.plain.get(), old.flat.get());
            let mut old = old.into_vec(0);
            unsafe {
                std::ptr::copy_nonoverlapping(old.as_ptr(), base.add(slack), len);
                old.set_len(0);
            }
            drop(old);
            let fresh = ManuallyDrop::new(fresh);
            *self = PackedVec {
                ptr: unsafe { base.add(slack) },
                len: u32_len(len),
                cap: u32_len(fresh.capacity() - slack),
                head: u32_len(slack),
                plain: Cell::new(plain),
                flat: Cell::new(flat),
                inline: false,
            };
        }
        unsafe {
            let start = self.ptr.sub(n);
            for (k, p) in items.take(n).enumerate() {
                self.note(&p);
                start.add(k).write(p);
            }
            self.ptr = start;
        }
        let n = n as u32;
        self.len += n;
        self.cap += n;
        self.head -= n;
    }

    /// Replace `del` elements at `start` with `items`, returning the removed ones. At the front
    /// this is slack bookkeeping; elsewhere one move of the tail.
    pub(in crate::value) fn splice(
        &mut self,
        start: usize,
        del: usize,
        items: impl ExactSizeIterator<Item = Elem>,
    ) -> Vec<Elem> {
        debug_assert!(start + del <= self.len as usize);
        if start == 0 {
            let removed = (0..del).filter_map(|_| self.pop_front()).collect();
            self.prepend(items);
            return removed;
        }
        let items: Vec<Elem> = items.collect();
        items.iter().for_each(|p| self.note(p));
        self.with_vec(0, |v| v.splice(start..start + del, items).collect())
    }

    pub(in crate::value) fn truncate(&mut self, n: usize) {
        if n >= self.len as usize {
            return;
        }
        let (tail, count) = (unsafe { self.ptr.add(n) }, self.len as usize - n);
        self.len = n as u32;
        if !self.flat.get() {
            unsafe { drop_elems(tail, count) };
        }
    }

    pub(in crate::value) fn resize_with(&mut self, n: usize, mut f: impl FnMut() -> Elem) {
        if n <= self.len as usize {
            self.truncate(n);
            return;
        }
        self.reserve(n - self.len as usize);
        while (self.len as usize) < n {
            self.push(f());
        }
    }

    pub(in crate::value) fn extend(&mut self, items: impl Iterator<Item = Elem>) {
        self.reserve(items.size_hint().0);
        for p in items {
            self.push(p);
        }
    }
}

impl From<Vec<Elem>> for PackedVec {
    fn from(v: Vec<Elem>) -> PackedVec {
        let mut v = ManuallyDrop::new(v);
        PackedVec {
            ptr: v.as_mut_ptr(),
            len: u32_len(v.len()),
            cap: u32_len(v.capacity()),
            head: 0,
            // Unknown until proved (a scan on first need), except for nothing at all.
            plain: Cell::new(v.is_empty()),
            flat: Cell::new(v.is_empty()),
            inline: false,
        }
    }
}

impl PackedVec {
    /// `v`, whose elements the caller has checked are all present (no holes).
    pub(in crate::value) fn from_plain(v: Vec<Elem>) -> PackedVec {
        let p = PackedVec::from(v);
        p.plain.set(true);
        p
    }
}

impl Default for PackedVec {
    fn default() -> PackedVec {
        PackedVec::from(Vec::new())
    }
}

impl Clone for PackedVec {
    /// A copy is always a heap buffer (in-box slots belong to their box).
    fn clone(&self) -> PackedVec {
        if self.is_none() {
            return PackedVec::NONE;
        }
        if self.flat.get() {
            return self.copy_flat(0, self.len as usize);
        }
        PackedVec::from(self.to_vec())
    }
}

impl Drop for PackedVec {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        unsafe {
            if !self.flat.get() {
                drop_elems(self.ptr, self.len as usize);
            }
            if !self.inline {
                let base = self.ptr.sub(self.head as usize);
                drop(Vec::from_raw_parts(
                    base,
                    0,
                    (self.cap + self.head) as usize,
                ));
            }
        }
    }
}

impl std::ops::Deref for PackedVec {
    type Target = [Elem];
    #[inline]
    fn deref(&self) -> &[Elem] {
        debug_assert!(!self.ptr.is_null());
        unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
    }
}

impl std::ops::DerefMut for PackedVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Elem] {
        debug_assert!(!self.ptr.is_null());
        self.plain.set(false);
        self.flat.set(false);
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len as usize) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn nums(p: &PackedVec) -> Vec<f64> {
        p.iter().map(|p| p.as_num().unwrap_or(f64::NAN)).collect()
    }
    fn prop(n: f64) -> Elem {
        Elem::pack(Value::Num(n))
    }

    #[test]
    fn queue_and_deque_operations() {
        let mut p = PackedVec::default();
        for k in 0..10 {
            p.push(prop(k as f64));
        }
        for k in 0..10_000 {
            assert!(p.pop_front().is_some());
            p.push(prop((k + 10) as f64));
        }
        assert_eq!(
            nums(&p),
            (10_000..10_010).map(|k| k as f64).collect::<Vec<_>>()
        );
        p.prepend([prop(-2.0), prop(-1.0)].into_iter());
        assert_eq!(nums(&p)[..3], [-2.0, -1.0, 10_000.0]);
        for k in 0..1000 {
            p.prepend(std::iter::once(prop(-(k as f64) - 3.0)));
        }
        assert_eq!(p.len(), 1012);
        assert_eq!(nums(&p)[0], -1002.0);
        p.truncate(5);
        let q = p.clone();
        assert_eq!(nums(&q), nums(&p));
        while p.pop_front().is_some() {}
        assert!(p.is_empty());
        p.resize_with(3, || prop(7.0));
        assert_eq!(nums(&p), [7.0, 7.0, 7.0]);
    }

    #[test]
    fn inline_slots_spill_on_growth() {
        let mut slots = [const { std::mem::MaybeUninit::<Elem>::uninit() }; 4];
        let buf = slots.as_mut_ptr().cast::<Elem>();
        let mut p = unsafe { PackedVec::inline_raw(buf, 0, 4) };
        for k in 0..3 {
            p.push(prop(k as f64));
        }
        assert!(p.inline && p.ptr == buf);
        assert!(p.pop_front().is_some_and(|p| p.as_num() == Some(0.0)));
        p.prepend(std::iter::once(prop(-1.0)));
        assert!(p.inline);
        for k in 3..12 {
            p.push(prop(k as f64));
        }
        assert!(!p.inline);
        assert_eq!(
            nums(&p),
            [
                -1.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0
            ]
        );
        let mut q = unsafe { PackedVec::inline_raw(buf, 0, 4) };
        q.push(prop(1.0));
        q.prepend([prop(-3.0), prop(-2.0)].into_iter());
        assert!(!q.inline);
        assert_eq!(nums(&q), [-3.0, -2.0, 1.0]);
    }
}
