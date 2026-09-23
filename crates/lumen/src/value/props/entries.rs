//! The property-entry vector: a 16-byte thin vector (pointer, `u32` length, `u32` capacity)
//! that can also *share* a read-only block with other maps.
//!
//! Shared mode (`cap == 0 && len > 0`): `ptr` points into a refcounted block whose entries are
//! never mutated in place; the first mutable access copies them out into an owned allocation
//! (copy-on-write). Closures use it: every instance of one function shares its template's
//! `length`/`name` entries, so the ordinary closure allocates no entry storage at all. Owned
//! mode is a plain vector.
use crate::value::Property;
use std::alloc::{alloc, dealloc, handle_alloc_error, realloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

/// Header of a shared block; the entries follow it (its size keeps them 16-byte aligned).
#[repr(C, align(16))]
struct SharedHeader {
    strong: Cell<usize>,
    len: usize,
}

const HEADER: usize = std::mem::size_of::<SharedHeader>();
const ENTRY: usize = std::mem::size_of::<Property>();

pub(in crate::value) struct EntryVec {
    ptr: NonNull<Property>,
    len: u32,
    cap: u32,
}

impl EntryVec {
    pub(in crate::value) const fn new() -> EntryVec {
        EntryVec {
            ptr: NonNull::dangling(),
            len: 0,
            cap: 0,
        }
    }

    pub(in crate::value) fn with_capacity(cap: usize) -> EntryVec {
        let mut v = EntryVec::new();
        if cap != 0 {
            v.grow_to(cap);
        }
        v
    }

    fn owned_layout(cap: usize) -> Layout {
        Layout::array::<Property>(cap).expect("entry vector too large")
    }

    fn shared_layout(len: usize) -> Layout {
        Layout::from_size_align(HEADER + len * ENTRY, 16).expect("entry block too large")
    }

    #[inline]
    pub(in crate::value) fn is_shared(&self) -> bool {
        self.cap == 0 && self.len != 0
    }

    #[inline]
    fn shared_header(&self) -> &SharedHeader {
        debug_assert!(self.is_shared());
        unsafe { &*((self.ptr.as_ptr() as *const u8).sub(HEADER) as *const SharedHeader) }
    }

    /// Two vectors sharing the same block.
    pub(in crate::value) fn shares_with(&self, other: &EntryVec) -> bool {
        self.is_shared() && other.is_shared() && self.ptr == other.ptr
    }

    /// Move this vector's entries into a shared block, so clones share instead of copying.
    /// No-op when already shared.
    pub(in crate::value) fn make_shared(&mut self) {
        if self.is_shared() || self.len == 0 {
            return;
        }
        let len = self.len as usize;
        let layout = Self::shared_layout(len);
        let block = unsafe { alloc(layout) };
        if block.is_null() {
            handle_alloc_error(layout);
        }
        unsafe {
            block.cast::<SharedHeader>().write(SharedHeader {
                strong: Cell::new(1),
                len,
            });
            let entries = block.add(HEADER).cast::<Property>();
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), entries, len);
            dealloc(
                self.ptr.as_ptr().cast(),
                Self::owned_layout(self.cap as usize),
            );
            self.ptr = NonNull::new_unchecked(entries);
        }
        self.cap = 0;
    }

    /// Reallocate the owned buffer to exactly `cap` entries (`cap >= len`).
    fn grow_to(&mut self, cap: usize) {
        debug_assert!(!self.is_shared());
        debug_assert!(cap >= self.len as usize);
        let cap_u32: u32 = cap.try_into().expect("entry vector exceeds u32 capacity");
        let new_layout = Self::owned_layout(cap);
        let p = unsafe {
            if self.cap == 0 {
                alloc(new_layout)
            } else {
                realloc(
                    self.ptr.as_ptr().cast(),
                    Self::owned_layout(self.cap as usize),
                    new_layout.size(),
                )
            }
        };
        if p.is_null() {
            handle_alloc_error(new_layout);
        }
        self.ptr = unsafe { NonNull::new_unchecked(p.cast()) };
        self.cap = cap_u32;
    }

    /// Copy shared entries into an owned buffer with room for `extra` more.
    fn unshare(&mut self, extra: usize) {
        if !self.is_shared() {
            return;
        }
        let len = self.len as usize;
        let src = self.ptr;
        let header = self.shared_header() as *const SharedHeader;
        let mut owned = EntryVec::with_capacity(len + extra);
        for i in 0..len {
            unsafe {
                owned
                    .ptr
                    .as_ptr()
                    .add(i)
                    .write((*src.as_ptr().add(i)).clone())
            };
        }
        owned.len = self.len;
        unsafe { Self::release_shared(header, src) };
        self.ptr = owned.ptr;
        self.cap = owned.cap;
        std::mem::forget(owned);
    }

    unsafe fn release_shared(header: *const SharedHeader, entries: NonNull<Property>) {
        let h = &*header;
        let strong = h.strong.get() - 1;
        h.strong.set(strong);
        if strong == 0 {
            let len = h.len;
            for i in 0..len {
                std::ptr::drop_in_place(entries.as_ptr().add(i));
            }
            dealloc(header as *mut u8, Self::shared_layout(len));
        }
    }

    #[inline]
    pub(in crate::value) fn len(&self) -> usize {
        self.len as usize
    }

    /// Owned capacity (zero while shared).
    #[inline]
    pub(in crate::value) fn capacity(&self) -> usize {
        self.cap as usize
    }

    pub(in crate::value) fn reserve_exact(&mut self, additional: usize) {
        if self.is_shared() {
            self.unshare(additional);
            return;
        }
        let need = self.len as usize + additional;
        if need > self.cap as usize {
            self.grow_to(need);
        }
    }

    pub(in crate::value) fn push(&mut self, prop: Property) {
        if self.is_shared() {
            self.unshare(1);
        } else if self.len == self.cap {
            let cap = self.cap as usize;
            self.grow_to(if cap < 2 { cap + 1 } else { cap * 2 });
        }
        unsafe { self.ptr.as_ptr().add(self.len as usize).write(prop) };
        self.len += 1;
    }

    pub(in crate::value) fn pop(&mut self) -> Option<Property> {
        if self.len == 0 {
            return None;
        }
        self.unshare(0);
        self.len -= 1;
        Some(unsafe { self.ptr.as_ptr().add(self.len as usize).read() })
    }

    pub(in crate::value) fn remove(&mut self, index: usize) -> Property {
        let len = self.len as usize;
        assert!(index < len, "entry index out of bounds");
        self.unshare(0);
        unsafe {
            let p = self.ptr.as_ptr().add(index);
            let out = p.read();
            std::ptr::copy(p.add(1), p, len - index - 1);
            self.len -= 1;
            out
        }
    }

    pub(in crate::value) fn clear(&mut self) {
        if self.is_shared() {
            unsafe { Self::release_shared(self.shared_header(), self.ptr) };
            self.ptr = NonNull::dangling();
            self.len = 0;
            return;
        }
        let len = self.len as usize;
        self.len = 0;
        for i in 0..len {
            unsafe { std::ptr::drop_in_place(self.ptr.as_ptr().add(i)) };
        }
    }

    pub(in crate::value) fn retain(&mut self, mut keep: impl FnMut(&Property) -> bool) {
        self.unshare(0);
        let len = self.len as usize;
        let base = self.ptr.as_ptr();
        let mut out = 0usize;
        for i in 0..len {
            unsafe {
                let p = base.add(i);
                if keep(&*p) {
                    if out != i {
                        std::ptr::copy_nonoverlapping(p, base.add(out), 1);
                    }
                    out += 1;
                } else {
                    std::ptr::drop_in_place(p);
                }
            }
        }
        self.len = out as u32;
    }

    #[inline]
    pub(in crate::value) fn get(&self, index: usize) -> Option<&Property> {
        (index < self.len as usize).then(|| unsafe { &*self.ptr.as_ptr().add(index) })
    }

    #[inline]
    pub(in crate::value) fn get_mut(&mut self, index: usize) -> Option<&mut Property> {
        if index >= self.len as usize {
            return None;
        }
        self.unshare(0);
        Some(unsafe { &mut *self.ptr.as_ptr().add(index) })
    }

    #[inline]
    pub(in crate::value) fn iter(&self) -> std::slice::Iter<'_, Property> {
        self.as_slice().iter()
    }

    #[inline]
    fn as_slice(&self) -> &[Property] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len as usize) }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [Property] {
        debug_assert!(!self.is_shared());
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl std::ops::Deref for EntryVec {
    type Target = [Property];
    #[inline]
    fn deref(&self) -> &[Property] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for EntryVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Property] {
        self.unshare(0);
        self.as_mut_slice()
    }
}

impl Clone for EntryVec {
    fn clone(&self) -> EntryVec {
        if self.is_shared() {
            let h = self.shared_header();
            h.strong.set(h.strong.get() + 1);
            return EntryVec {
                ptr: self.ptr,
                len: self.len,
                cap: 0,
            };
        }
        let mut v = EntryVec::with_capacity(self.len as usize);
        for p in self.iter() {
            v.push(p.clone());
        }
        v
    }
}

impl Drop for EntryVec {
    fn drop(&mut self) {
        self.clear();
        if self.cap != 0 {
            unsafe {
                dealloc(
                    self.ptr.as_ptr().cast(),
                    Self::owned_layout(self.cap as usize),
                )
            };
        }
    }
}

impl<I: std::slice::SliceIndex<[Property]>> std::ops::Index<I> for EntryVec {
    type Output = I::Output;
    #[inline]
    fn index(&self, index: I) -> &I::Output {
        &self.as_slice()[index]
    }
}

impl<I: std::slice::SliceIndex<[Property]>> std::ops::IndexMut<I> for EntryVec {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        self.unshare(0);
        &mut self.as_mut_slice()[index]
    }
}

impl FromIterator<Property> for EntryVec {
    fn from_iter<T: IntoIterator<Item = Property>>(iter: T) -> EntryVec {
        let iter = iter.into_iter();
        let mut v = EntryVec::with_capacity(iter.size_hint().0);
        for p in iter {
            v.push(p);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn num(f: f64) -> Property {
        Property::plain(Value::Num(f))
    }

    fn as_num(p: &Property) -> f64 {
        match p.value() {
            Value::Num(f) => f,
            _ => f64::NAN,
        }
    }

    fn nums(v: &EntryVec) -> Vec<f64> {
        v.iter().map(as_num).collect()
    }

    #[test]
    fn layout_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<EntryVec>(), 16);
    }

    #[test]
    fn owned_push_pop_remove_retain() {
        let mut v = EntryVec::new();
        for i in 0..5 {
            v.push(num(i as f64));
        }
        assert_eq!(nums(&v), [0., 1., 2., 3., 4.]);
        assert_eq!(as_num(&v.remove(1)), 1.0);
        assert_eq!(nums(&v), [0., 2., 3., 4.]);
        v.retain(|p| !matches!(p.value(), Value::Num(f) if f == 3.0));
        assert_eq!(nums(&v), [0., 2., 4.]);
        assert_eq!(v.pop().as_ref().map(as_num), Some(4.0));
        assert_eq!(v.len(), 2);
        v.clear();
        assert!(v.is_empty());
    }

    #[test]
    fn shared_clone_copies_on_write() {
        let mut template = EntryVec::new();
        template.push(Property::plain(Value::str("a")));
        template.push(num(2.0));
        template.make_shared();
        assert!(template.is_shared());
        let a = template.clone();
        let mut b = template.clone();
        assert!(a.shares_with(&template) && b.shares_with(&template));
        assert_eq!(template.shared_header().strong.get(), 3);
        assert_eq!(a.capacity(), 0);
        b.push(num(3.0));
        assert!(!b.is_shared());
        assert_eq!(b.len(), 3);
        assert_eq!(template.len(), 2);
        assert_eq!(template.shared_header().strong.get(), 2);
        b[0] = num(9.0);
        assert!(matches!(a[0].value(), Value::Str(_)));
        drop(a);
        drop(template);
        assert_eq!(b.len(), 3);
        let mut c = EntryVec::new();
        c.push(num(1.0));
        c.make_shared();
        let mut d = c.clone();
        assert_eq!(as_num(&d.remove(0)), 1.0);
        assert!(d.is_empty());
        assert_eq!(c.len(), 1);
        d.clear();
        c.clear();
        assert!(c.is_empty() && !c.is_shared());
    }
}
#[cfg(test)]
mod object_layout {
    #[test]
    fn object_header_fits_the_96_byte_malloc_class() {
        use std::mem::size_of;
        assert_eq!(size_of::<crate::value::Props>(), 40);
        assert_eq!(size_of::<crate::value::Exotic>(), 1);
        assert_eq!(size_of::<crate::value::Callable>(), 16);
        assert!(size_of::<crate::value::Object>() <= 72);
    }
}
