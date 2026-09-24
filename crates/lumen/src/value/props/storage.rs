//! Optional dense buffers and inline packed property ownership.
use super::{MIRROR_ALL_I32, MIRROR_NO_HOLES, MIRROR_OK};
use crate::value::{Property, Value};
pub(in crate::value) const INLINE_PACKED_CAPACITY: usize = 10;

pub(super) struct InlinePacked {
    pub(in crate::value) len: u8,
    pub(in crate::value) slots: [std::mem::MaybeUninit<Property>; INLINE_PACKED_CAPACITY],
}

impl InlinePacked {
    pub(in crate::value) const EMPTY: InlinePacked = InlinePacked {
        len: 0,
        slots: [const { std::mem::MaybeUninit::uninit() }; INLINE_PACKED_CAPACITY],
    };

    #[cfg(test)]
    pub(super) fn from_values(values: impl ExactSizeIterator<Item = Value>) -> InlinePacked {
        assert!(values.len() <= INLINE_PACKED_CAPACITY);
        let mut packed = InlinePacked::default();
        for value in values {
            let index = packed.len as usize;
            assert!(index < INLINE_PACKED_CAPACITY);
            packed.slots[index].write(Property::plain(value));
            // Count every initialized owner immediately, including during unwinding.
            packed.len += 1;
        }
        packed
    }

    pub(in crate::value) fn as_slice(&self) -> &[Property] {
        unsafe {
            std::slice::from_raw_parts(self.slots.as_ptr().cast::<Property>(), self.len as usize)
        }
    }

    pub(in crate::value) fn into_vec(&mut self) -> Vec<Property> {
        let len = self.len as usize;
        let mut values = Vec::with_capacity(len);
        for index in 0..len {
            values.push(unsafe { self.slots[index].assume_init_read() });
        }
        self.len = 0;
        values
    }
}

impl Default for InlinePacked {
    fn default() -> Self {
        InlinePacked::EMPTY
    }
}

impl Clone for InlinePacked {
    fn clone(&self) -> Self {
        let mut clone = InlinePacked::default();
        for (index, property) in self.as_slice().iter().enumerate() {
            clone.slots[index].write(property.clone());
        }
        clone.len = self.len;
        clone
    }
}

impl Drop for InlinePacked {
    fn drop(&mut self) {
        for index in 0..self.len as usize {
            unsafe { self.slots[index].assume_init_drop() };
        }
    }
}

pub(in crate::value) struct DenseBuffers {
    pub(in crate::value) packed: Option<Box<Vec<Property>>>,
    pub(super) inline_packed: InlinePacked,
    pub(in crate::value) elems: Vec<u32>,
    pub(in crate::value) mirror: Vec<f64>,
    /// See `Props::mirror`: [`MIRROR_OK`] | [`MIRROR_ALL_I32`] | [`MIRROR_NO_HOLES`]. A map
    /// without a sidecar has no elements, so its mirror is vacuously coherent.
    pub(in crate::value) mirror_flags: u8,
    /// Live hole count in `mirror` (descending array fills pad with holes and then fill them:
    /// `MIRROR_NO_HOLES` comes back when this returns to zero).
    pub(in crate::value) mirror_holes: u32,
    /// These buffers live in their object's own heap box (`heap::SlotClass::Array`), not in a
    /// separate allocation: see [`DenseStorage`].
    pub(in crate::value) in_box: bool,
}

impl Clone for DenseBuffers {
    /// A copy is always a separate allocation's contents (`in_box` clear).
    fn clone(&self) -> Self {
        DenseBuffers {
            packed: self.packed.clone(),
            inline_packed: self.inline_packed.clone(),
            elems: self.elems.clone(),
            mirror: self.mirror.clone(),
            mirror_flags: self.mirror_flags,
            mirror_holes: self.mirror_holes,
            in_box: false,
        }
    }
}

pub(super) const MIRROR_DEFAULT: u8 = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;

impl Default for DenseBuffers {
    fn default() -> Self {
        DenseBuffers {
            packed: None,
            inline_packed: InlinePacked::default(),
            elems: Vec::new(),
            mirror: Vec::new(),
            mirror_flags: MIRROR_DEFAULT,
            mirror_holes: 0,
            in_box: false,
        }
    }
}

struct EmptyDenseBuffers(DenseBuffers);
// This one value contains only `None` and empty Vec dangling sentinels and is never mutated; no
// non-Sync payload is reachable through it. Live DenseBuffers remain thread-local as before.
unsafe impl Sync for EmptyDenseBuffers {}

static EMPTY_DENSE_BUFFERS: EmptyDenseBuffers = EmptyDenseBuffers(DenseBuffers {
    packed: None,
    inline_packed: InlinePacked::EMPTY,
    elems: Vec::new(),
    mirror: Vec::new(),
    mirror_flags: MIRROR_DEFAULT,
    mirror_holes: 0,
    in_box: false,
});

/// The nullable sidecar pointer. Usually it owns a boxed [`DenseBuffers`]; a small array
/// literal's buffers instead live in the tail of its object's own heap box
/// ([`DenseBuffers::in_box`], installed by [`DenseStorage::init_in_box`]), so the array is one
/// allocation. Like inline entry storage (see `EntryVec`), in-box buffers belong to the box:
/// the map holding them must stay inside that object — nothing moves a map out of a live
/// object, and a clone copies the buffers into a fresh allocation.
#[repr(transparent)]
#[derive(Default)]
pub(in crate::value) struct DenseStorage(Option<std::ptr::NonNull<DenseBuffers>>);

impl std::ops::Deref for DenseStorage {
    type Target = DenseBuffers;
    fn deref(&self) -> &DenseBuffers {
        self.as_deref().unwrap_or(&EMPTY_DENSE_BUFFERS.0)
    }
}

impl Clone for DenseStorage {
    fn clone(&self) -> DenseStorage {
        DenseStorage::from_box(self.as_deref().map(|d| Box::new(d.clone())))
    }
}

impl Drop for DenseStorage {
    fn drop(&mut self) {
        self.release();
    }
}

impl DenseStorage {
    #[inline]
    pub(in crate::value) fn from_box(b: Option<Box<DenseBuffers>>) -> DenseStorage {
        DenseStorage(b.map(|b| unsafe { std::ptr::NonNull::new_unchecked(Box::into_raw(b)) }))
    }

    /// Install packed buffers holding `values` at `slot` (as [`init_in_box`]), written in
    /// place: up to [`INLINE_PACKED_CAPACITY`] elements in the buffers' own slots, a longer run
    /// in one exactly-sized boxed vector.
    ///
    /// # Safety
    /// As [`init_in_box`](DenseStorage::init_in_box).
    #[inline]
    pub(in crate::value) unsafe fn adopt_in_box_packed(
        &mut self,
        slot: *mut DenseBuffers,
        values: impl ExactSizeIterator<Item = Value>,
    ) {
        let len = values.len();
        if len > INLINE_PACKED_CAPACITY {
            let mut packed = Vec::with_capacity(len);
            packed.extend(values.map(Property::plain));
            self.init_in_box(slot, Some(Box::new(packed)));
            return;
        }
        self.init_in_box(slot, None);
        // Adopted first, so the object's drop releases whatever landed if `values` panics.
        let ip = std::ptr::addr_of_mut!((*slot).inline_packed);
        for v in values.take(INLINE_PACKED_CAPACITY) {
            let n = (*ip).len as usize;
            (*ip).slots[n].write(Property::plain(v));
            (*ip).len += 1;
        }
    }

    /// A new box's in-box sidecar holding `values` (at most [`INLINE_PACKED_CAPACITY`]) as
    /// its packed elements, written field by field at `slot`; returns the pointer to install
    /// (by value, inside the box write). The fast array allocation's form of
    /// [`adopt_in_box_packed`](DenseStorage::adopt_in_box_packed).
    ///
    /// # Safety
    /// As [`init_in_box`](DenseStorage::init_in_box); `values.len() <= INLINE_PACKED_CAPACITY`.
    #[inline(always)]
    pub(in crate::value) unsafe fn write_in_box_packed(
        slot: *mut DenseBuffers,
        values: impl ExactSizeIterator<Item = Value>,
    ) -> DenseStorage {
        use std::ptr::addr_of_mut;
        debug_assert!(values.len() <= INLINE_PACKED_CAPACITY);
        let ip = addr_of_mut!((*slot).inline_packed.slots).cast::<Property>();
        let mut n = 0;
        for v in values.take(INLINE_PACKED_CAPACITY) {
            ip.add(n).write(Property::plain(v));
            n += 1;
        }
        addr_of_mut!((*slot).packed).write(None);
        addr_of_mut!((*slot).inline_packed.len).write(n as u8);
        addr_of_mut!((*slot).elems).write(Vec::new());
        addr_of_mut!((*slot).mirror).write(Vec::new());
        addr_of_mut!((*slot).mirror_flags).write(0);
        addr_of_mut!((*slot).mirror_holes).write(0);
        addr_of_mut!((*slot).in_box).write(true);
        DenseStorage(Some(std::ptr::NonNull::new_unchecked(slot)))
    }

    /// Install empty buffers (plus the boxed packed vector `packed`, if any) at `slot`, field by
    /// field: the inline element slots stay uninitialized instead of being copied around.
    ///
    /// # Safety
    /// `slot` must be the uninitialized sidecar area of the heap box that contains `self`,
    /// valid for as long as that box's object lives.
    #[inline]
    #[allow(clippy::box_collection)] // a thin pointer keeps `DenseBuffers` small
    pub(in crate::value) unsafe fn init_in_box(
        &mut self,
        slot: *mut DenseBuffers,
        packed: Option<Box<Vec<Property>>>,
    ) {
        use std::ptr::addr_of_mut;
        self.release();
        addr_of_mut!((*slot).packed).write(packed);
        addr_of_mut!((*slot).inline_packed.len).write(0);
        addr_of_mut!((*slot).elems).write(Vec::new());
        addr_of_mut!((*slot).mirror).write(Vec::new());
        addr_of_mut!((*slot).mirror_flags).write(0);
        addr_of_mut!((*slot).mirror_holes).write(0);
        addr_of_mut!((*slot).in_box).write(true);
        self.0 = Some(std::ptr::NonNull::new_unchecked(slot));
    }

    /// Drop the sidecar (freeing it unless it lives in the object's box).
    #[inline]
    fn release(&mut self) {
        if let Some(p) = self.0.take() {
            unsafe {
                if (*p.as_ptr()).in_box {
                    std::ptr::drop_in_place(p.as_ptr());
                } else {
                    drop(Box::from_raw(p.as_ptr()));
                }
            }
        }
    }

    #[inline]
    pub(in crate::value) fn as_deref(&self) -> Option<&DenseBuffers> {
        self.0.map(|p| unsafe { &*p.as_ptr() })
    }
    #[inline]
    pub(in crate::value) fn as_deref_mut(&mut self) -> Option<&mut DenseBuffers> {
        self.0.map(|p| unsafe { &mut *p.as_ptr() })
    }

    pub(in crate::value) fn is_present(&self) -> bool {
        self.0.is_some()
    }
    #[inline]
    pub(in crate::value) fn buffers_mut(&mut self) -> &mut DenseBuffers {
        if self.0.is_none() {
            *self = DenseStorage::from_box(Some(Box::default()));
        }
        self.as_deref_mut().expect("sidecar installed above")
    }
    pub(in crate::value) fn packed_mut(&mut self) -> Option<&mut Vec<Property>> {
        let dense = self.as_deref_mut()?;
        if dense.packed.is_none() && dense.inline_packed.len != 0 {
            dense.packed = Some(Box::new(dense.inline_packed.into_vec()));
        }
        dense.packed.as_deref_mut()
    }
    pub(in crate::value) fn packed_ref(&self) -> Option<&[Property]> {
        let dense = self.as_deref()?;
        match dense.packed.as_deref() {
            Some(packed) => Some(packed),
            None if dense.inline_packed.len != 0 => Some(dense.inline_packed.as_slice()),
            None => None,
        }
    }
    pub(in crate::value) fn packed_is_some(&self) -> bool {
        self.packed_ref().is_some()
    }
    pub(in crate::value) fn set_packed(&mut self, packed: Option<Box<Vec<Property>>>) {
        if packed.is_some() {
            let dense = self.buffers_mut();
            dense.inline_packed = InlinePacked::default();
            dense.packed = packed;
        } else if let Some(d) = self.as_deref_mut() {
            d.packed = None;
            d.inline_packed = InlinePacked::default();
        }
    }
    #[inline]
    pub(in crate::value) fn len(&self) -> usize {
        self.as_deref().map_or(0, |d| d.elems.len())
    }
    #[inline]
    pub(in crate::value) fn get(&self, index: usize) -> Option<&u32> {
        self.as_deref().and_then(|d| d.elems.get(index))
    }
    #[inline]
    pub(in crate::value) fn get_mut(&mut self, index: usize) -> Option<&mut u32> {
        self.as_deref_mut().and_then(|d| d.elems.get_mut(index))
    }
    pub(in crate::value) fn reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.buffers_mut().elems.reserve_exact(additional);
        }
    }
    #[inline]
    pub(in crate::value) fn push(&mut self, value: u32) {
        self.buffers_mut().elems.push(value);
    }
    #[inline]
    pub(in crate::value) fn pop(&mut self) -> Option<u32> {
        self.as_deref_mut().and_then(|d| d.elems.pop())
    }
    pub(in crate::value) fn clear(&mut self) {
        self.release();
    }
    pub(in crate::value) fn clear_elems(&mut self) {
        if let Some(d) = self.as_deref_mut() {
            d.elems.clear();
            d.mirror.clear();
        }
    }
    pub(in crate::value) fn iter_mut(&mut self) -> std::slice::IterMut<'_, u32> {
        self.buffers_mut().elems.iter_mut()
    }

    pub(in crate::value) fn mirror_reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.buffers_mut().mirror.reserve_exact(additional);
        }
    }
    pub(in crate::value) fn mirror_len(&self) -> usize {
        self.as_deref().map_or(0, |d| d.mirror.len())
    }
    pub(in crate::value) fn mirror_get(&self, index: usize) -> Option<&f64> {
        self.as_deref().and_then(|d| d.mirror.get(index))
    }
    pub(in crate::value) fn mirror_get_mut(&mut self, index: usize) -> Option<&mut f64> {
        self.as_deref_mut().and_then(|d| d.mirror.get_mut(index))
    }
    pub(in crate::value) fn mirror_push(&mut self, value: f64) {
        self.buffers_mut().mirror.push(value);
    }
    pub(in crate::value) fn mirror_pop(&mut self) -> Option<f64> {
        self.as_deref_mut().and_then(|d| d.mirror.pop())
    }
    #[inline]
    pub(in crate::value) fn mirror_flags(&self) -> u8 {
        self.as_deref().map_or(MIRROR_DEFAULT, |d| d.mirror_flags)
    }
    /// Mutable flags; allocates the sidecar when absent (a caller clearing a vacuous mirror
    /// should use [`mirror_invalidate`](Props::mirror_invalidate) instead).
    #[inline]
    pub(in crate::value) fn mirror_flags_mut(&mut self) -> &mut u8 {
        &mut self.buffers_mut().mirror_flags
    }
    #[inline]
    pub(in crate::value) fn mirror_holes_mut(&mut self) -> &mut u32 {
        &mut self.buffers_mut().mirror_holes
    }
    /// Reset the mirror to the coherent, hole-free, all-i32 state (after the elements went).
    pub(in crate::value) fn mirror_reset(&mut self) {
        if let Some(d) = self.as_deref_mut() {
            d.mirror_flags = MIRROR_DEFAULT;
            d.mirror_holes = 0;
        }
    }
}

impl std::ops::Index<usize> for DenseStorage {
    type Output = u32;
    fn index(&self, index: usize) -> &u32 {
        self.get(index).expect("dense index out of bounds")
    }
}

impl std::ops::IndexMut<usize> for DenseStorage {
    fn index_mut(&mut self, index: usize) -> &mut u32 {
        self.get_mut(index).expect("dense index out of bounds")
    }
}
