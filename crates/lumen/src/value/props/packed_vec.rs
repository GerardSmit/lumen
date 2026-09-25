//! Growable packed element buffer with room at both ends.
//!
//! A `Vec<Property>` whose live run may start `head` slots into its allocation, so `shift` and
//! `unshift` move one pointer instead of every element: removing the first element advances
//! the start, and inserting before it reuses the slots freed that way (or opens a fresh run of
//! front slack). Indexed access is unchanged, and the JIT reads `ptr` and `len` exactly as it
//! read a `Vec`'s.
use crate::value::Property;
use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

#[repr(C)]
pub(in crate::value) struct PackedVec {
    /// First live element; `head` slots after the allocation start.
    ptr: NonNull<Property>,
    len: usize,
    /// Capacity counted from `ptr` (the allocation holds `head + cap`).
    cap: usize,
    /// Uninitialized slots in front of `ptr`.
    head: usize,
    /// Every live element is known to be a plain data property (writable, enumerable,
    /// configurable, not a hole). Cleared by any mutable element access that could change
    /// that; [`PackedVec::all_plain`] re-proves it with a scan.
    plain: Cell<bool>,
    /// Every live element is known to be plain and to hold no heap handle (numbers, booleans,
    /// `undefined`, `null`): copies are a `memcpy` and dropping needs no per-element work.
    /// Implies `plain`; [`PackedVec::all_flat`] re-proves it.
    flat: Cell<bool>,
}

/// Plain, and no handle to release or retain.
#[inline]
fn is_flat(p: &Property) -> bool {
    p.is_plain_element() && !p.needs_drop()
}

pub(in crate::value) const PACKED_PTR: usize = std::mem::offset_of!(PackedVec, ptr);
pub(in crate::value) const PACKED_LEN: usize = std::mem::offset_of!(PackedVec, len);

impl PackedVec {
    pub(in crate::value) fn with_capacity(n: usize) -> PackedVec {
        PackedVec::from(Vec::with_capacity(n))
    }

    /// The buffer as an ordinary vector: the live run is moved down to the allocation start.
    fn into_vec(self) -> Vec<Property> {
        let me = ManuallyDrop::new(self);
        unsafe {
            let base = me.ptr.as_ptr().sub(me.head);
            if me.head != 0 {
                std::ptr::copy(me.ptr.as_ptr(), base, me.len);
            }
            Vec::from_raw_parts(base, me.len, me.cap + me.head)
        }
    }

    /// Whether every element is a plain data property (see `plain`); a failed proof costs a
    /// scan, a successful one is remembered.
    pub(in crate::value) fn all_plain(&self) -> bool {
        if !self.plain.get() {
            self.plain.set(self.iter().all(Property::is_plain_element));
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
        let mut v: Vec<Property> = Vec::with_capacity(run.len());
        // SAFETY: flat properties own nothing, so a bitwise copy is a clone.
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
    fn note(&self, p: &Property) {
        let plain = p.is_plain_element();
        self.plain.set(self.plain.get() & plain);
        self.flat.set(self.flat.get() & plain & !p.needs_drop());
    }

    /// Element `n` for a value-only write (`set_value` of a non-hole): keeps the `plain` proof
    /// (the value may become a handle: `flat` is dropped).
    #[inline]
    pub(in crate::value) fn get_value_mut(&mut self, n: usize) -> Option<&mut Property> {
        if n < self.len {
            self.flat.set(false);
            Some(unsafe { &mut *self.ptr.as_ptr().add(n) })
        } else {
            None
        }
    }

    /// Run `f` on the buffer as a `Vec` (for operations that may reallocate).
    fn with_vec<R>(&mut self, f: impl FnOnce(&mut Vec<Property>) -> R) -> R {
        let (plain, flat) = (self.plain.get(), self.flat.get());
        let taken = std::mem::replace(self, PackedVec::default());
        let mut v = taken.into_vec();
        let r = f(&mut v);
        *self = PackedVec::from(v);
        self.plain.set(plain);
        self.flat.set(flat);
        r
    }

    /// Ensure room for `extra` more elements at the back. A buffer with front slack is
    /// compacted first, and then grown so that at least `len` slots stay free: a queue that
    /// shifts and pushes in turn pays for one move per `len` operations.
    #[inline(never)]
    pub(in crate::value) fn reserve(&mut self, extra: usize) {
        if self.cap - self.len >= extra {
            return;
        }
        let head = self.head;
        self.with_vec(|v| {
            let want = if head != 0 { extra.max(v.len()) } else { extra };
            v.reserve(want);
        });
    }

    #[inline]
    pub(in crate::value) fn push(&mut self, p: Property) {
        if self.len == self.cap {
            self.reserve(1);
        }
        self.note(&p);
        unsafe { self.ptr.as_ptr().add(self.len).write(p) };
        self.len += 1;
    }

    pub(in crate::value) fn pop(&mut self) -> Option<Property> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(unsafe { self.ptr.as_ptr().add(self.len).read() })
    }

    /// Remove and return the first element: O(1), the slot becomes front slack.
    pub(in crate::value) fn pop_front(&mut self) -> Option<Property> {
        if self.len == 0 {
            return None;
        }
        let p = unsafe { self.ptr.as_ptr().read() };
        self.ptr = unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(1)) };
        self.len -= 1;
        self.cap -= 1;
        self.head += 1;
        // Give most of a drained front back once it dwarfs what is live.
        if self.head >= 64 && self.head > 4 * self.len {
            self.with_vec(|v| v.shrink_to(v.len() * 2));
        }
        Some(p)
    }

    /// Insert `items` before the first element, in order. Uses front slack when there is
    /// enough, else moves the run once into a buffer with `len` slots of new front slack.
    pub(in crate::value) fn prepend(&mut self, items: impl ExactSizeIterator<Item = Property>) {
        let n = items.len();
        if n == 0 {
            return;
        }
        if self.head < n {
            let slack = n.max(self.len).max(4);
            let mut fresh: Vec<Property> = Vec::with_capacity(slack + self.cap);
            let base = fresh.as_mut_ptr();
            let old = std::mem::replace(self, PackedVec::default());
            let (len, plain, flat) = (old.len, old.plain.get(), old.flat.get());
            let mut old = old.into_vec();
            unsafe {
                std::ptr::copy_nonoverlapping(old.as_ptr(), base.add(slack), len);
                old.set_len(0);
            }
            drop(old);
            let fresh = ManuallyDrop::new(fresh);
            *self = PackedVec {
                ptr: unsafe { NonNull::new_unchecked(base.add(slack)) },
                len,
                cap: fresh.capacity() - slack,
                head: slack,
                plain: Cell::new(plain),
                flat: Cell::new(flat),
            };
        }
        unsafe {
            let start = self.ptr.as_ptr().sub(n);
            for (k, p) in items.take(n).enumerate() {
                self.note(&p);
                start.add(k).write(p);
            }
            self.ptr = NonNull::new_unchecked(start);
        }
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
        items: impl ExactSizeIterator<Item = Property>,
    ) -> Vec<Property> {
        debug_assert!(start + del <= self.len);
        if start == 0 {
            let removed = (0..del).filter_map(|_| self.pop_front()).collect();
            self.prepend(items);
            return removed;
        }
        let items: Vec<Property> = items.collect();
        items.iter().for_each(|p| self.note(p));
        self.with_vec(|v| v.splice(start..start + del, items).collect())
    }

    pub(in crate::value) fn truncate(&mut self, n: usize) {
        if n >= self.len {
            return;
        }
        let (tail, count) = (unsafe { self.ptr.as_ptr().add(n) }, self.len - n);
        self.len = n;
        if !self.flat.get() {
            unsafe { crate::value::drop_properties(tail, count) };
        }
    }

    pub(in crate::value) fn resize_with(&mut self, n: usize, mut f: impl FnMut() -> Property) {
        if n <= self.len {
            self.truncate(n);
            return;
        }
        self.reserve(n - self.len);
        while self.len < n {
            self.push(f());
        }
    }

    pub(in crate::value) fn extend(&mut self, items: impl Iterator<Item = Property>) {
        self.reserve(items.size_hint().0);
        for p in items {
            self.push(p);
        }
    }
}

impl From<Vec<Property>> for PackedVec {
    fn from(v: Vec<Property>) -> PackedVec {
        let mut v = ManuallyDrop::new(v);
        PackedVec {
            ptr: unsafe { NonNull::new_unchecked(v.as_mut_ptr()) },
            len: v.len(),
            cap: v.capacity(),
            head: 0,
            // Unknown until proved (a scan on first need), except for nothing at all.
            plain: Cell::new(v.is_empty()),
            flat: Cell::new(v.is_empty()),
        }
    }
}

impl PackedVec {
    /// `v`, whose elements the caller has checked are all plain data elements.
    pub(in crate::value) fn from_plain(v: Vec<Property>) -> PackedVec {
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
    fn clone(&self) -> PackedVec {
        if self.flat.get() {
            return self.copy_flat(0, self.len);
        }
        PackedVec::from(self.to_vec())
    }
}

impl Drop for PackedVec {
    fn drop(&mut self) {
        unsafe {
            if !self.flat.get() {
                crate::value::drop_properties(self.ptr.as_ptr(), self.len);
            }
            let base = self.ptr.as_ptr().sub(self.head);
            drop(Vec::from_raw_parts(base, 0, self.cap + self.head));
        }
    }
}

impl std::ops::Deref for PackedVec {
    type Target = [Property];
    #[inline]
    fn deref(&self) -> &[Property] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for PackedVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Property] {
        self.plain.set(false);
        self.flat.set(false);
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn nums(p: &PackedVec) -> Vec<f64> {
        p.iter().map(|p| match p.value() { Value::Num(n) => n, _ => f64::NAN }).collect()
    }
    fn prop(n: f64) -> Property {
        Property::plain(Value::Num(n))
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
        assert_eq!(nums(&p), (10_000..10_010).map(|k| k as f64).collect::<Vec<_>>());
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
}

