//! The dense element sidecar: packed elements, or the classic slot map and numeric mirror.
use super::PackedVec;
use super::{MIRROR_ALL_I32, MIRROR_NO_HOLES, MIRROR_OK};
use crate::value::{PackedValue, Value};
use std::ptr::NonNull;

/// The classic element representation (sparse, holey and numeric-mirror arrays and index keys
/// on ordinary objects): `elems[n]` is the `entries` slot of canonical-index key `n`, and the
/// raw-f64 `mirror` (see `Props::mirror`). Allocated only once a map uses it.
#[derive(Clone)]
pub(in crate::value) struct Classic {
    pub(in crate::value) elems: Vec<u32>,
    pub(in crate::value) mirror: Vec<f64>,
    /// Live hole count in `mirror` (descending array fills pad with holes and then fill them:
    /// `MIRROR_NO_HOLES` comes back when this returns to zero).
    pub(in crate::value) mirror_holes: u32,
    /// See `Props::mirror`: [`MIRROR_OK`] | [`MIRROR_ALL_I32`] | [`MIRROR_NO_HOLES`].
    pub(in crate::value) mirror_flags: u8,
}

impl Default for Classic {
    fn default() -> Self {
        Classic {
            elems: Vec::new(),
            mirror: Vec::new(),
            mirror_holes: 0,
            mirror_flags: MIRROR_DEFAULT,
        }
    }
}

/// An element sidecar. Packed mode (`packed` present) keeps the elements themselves, in order;
/// otherwise the elements are `entries` slots addressed through `classic`.
#[repr(C)]
pub(in crate::value) struct DenseBuffers {
    /// The packed elements, or [`PackedVec::NONE`].
    pub(in crate::value) packed: PackedVec,
    pub(in crate::value) classic: Option<Box<Classic>>,
}

impl Clone for DenseBuffers {
    /// A copy owns heap buffers only (in-box element slots stay with their box).
    fn clone(&self) -> Self {
        DenseBuffers {
            packed: self.packed.clone(),
            classic: self.classic.clone(),
        }
    }
}

/// The mirror state of a map without classic storage: no elements, so vacuously coherent.
pub(super) const MIRROR_DEFAULT: u8 = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;

const EMPTY_BUFFERS: DenseBuffers = DenseBuffers {
    packed: PackedVec::NONE,
    classic: None,
};

struct EmptyDenseBuffers(DenseBuffers);
// This one value holds a null packed pointer and no classic storage and is never mutated; no
// non-Sync payload is reachable through it. Live DenseBuffers remain thread-local as before.
unsafe impl Sync for EmptyDenseBuffers {}

static EMPTY_DENSE_BUFFERS: EmptyDenseBuffers = EmptyDenseBuffers(EMPTY_BUFFERS);

/// Tag bit of a [`DenseStorage`] word that owns a separate (boxed) allocation.
pub(in crate::value) const DENSE_BOXED: usize = 1;

const _: () = assert!(std::mem::align_of::<DenseBuffers>() > DENSE_BOXED);
// In-box element slots follow the buffers directly.
const _: () =
    assert!(std::mem::size_of::<DenseBuffers>() % std::mem::align_of::<PackedValue>() == 0);

/// The nullable sidecar pointer. Usually an array's buffers live in the tail of its object's
/// own heap box, followed by a few element slots its packed storage starts out in
/// ([`DenseStorage::write_in_box`]), so a small array is one allocation; any other map that
/// gains elements owns a boxed [`DenseBuffers`] instead, marked by the [`DENSE_BOXED`] bit.
/// Like inline entry storage (see `EntryVec`), in-box buffers belong to the box: the map
/// holding them must stay inside that object — nothing moves a map out of a live object, and a
/// clone copies the buffers into a fresh allocation.
#[repr(transparent)]
#[derive(Default)]
pub(in crate::value) struct DenseStorage(Option<NonNull<DenseBuffers>>);

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

/// The element slots trailing in-box buffers at `slot`.
#[inline(always)]
fn in_box_slots(slot: *mut DenseBuffers) -> *mut PackedValue {
    unsafe { slot.add(1).cast::<PackedValue>() }
}

impl DenseStorage {
    #[inline]
    pub(in crate::value) fn from_box(b: Option<Box<DenseBuffers>>) -> DenseStorage {
        DenseStorage(b.map(|b| unsafe {
            NonNull::new_unchecked(Box::into_raw(b).map_addr(|a| a | DENSE_BOXED))
        }))
    }

    #[inline(always)]
    fn raw(&self) -> Option<*mut DenseBuffers> {
        self.0.map(|p| p.as_ptr().map_addr(|a| a & !DENSE_BOXED))
    }

    /// New in-box buffers at `slot` holding `values` as packed elements: in the `k` element
    /// slots that follow `slot` when they fit, else in one exactly-sized heap buffer. Written
    /// field by field; returns the pointer to install (by value, inside the box write).
    ///
    /// # Safety
    /// `slot` must be the uninitialized sidecar area of the heap box the returned storage is
    /// written into, followed by `k` element slots, valid for as long as that box's object
    /// lives.
    #[inline(always)]
    pub(in crate::value) unsafe fn write_in_box(
        slot: *mut DenseBuffers,
        k: usize,
        values: impl ExactSizeIterator<Item = Value>,
    ) -> DenseStorage {
        let len = values.len();
        let packed = if len <= k {
            let buf = in_box_slots(slot);
            let mut n = 0;
            for v in values.take(k) {
                buf.add(n).write(PackedValue::pack(v));
                n += 1;
            }
            PackedVec::inline_raw(buf, n, k)
        } else {
            let mut v = Vec::with_capacity(len);
            v.extend(values.map(PackedValue::pack));
            PackedVec::from(v)
        };
        slot.write(DenseBuffers {
            packed,
            classic: None,
        });
        DenseStorage(Some(NonNull::new_unchecked(slot)))
    }

    /// Install in-box buffers at `slot` (as [`write_in_box`](DenseStorage::write_in_box)) and
    /// append `values` to their packed elements. The buffers are adopted first, so the
    /// object's drop releases whatever landed if `values` panics.
    ///
    /// # Safety
    /// As [`write_in_box`](DenseStorage::write_in_box), and `self` must live in that box.
    pub(in crate::value) unsafe fn adopt_in_box(
        &mut self,
        slot: *mut DenseBuffers,
        k: usize,
        values: impl ExactSizeIterator<Item = Value>,
    ) {
        let len = values.len();
        if len <= k {
            self.install_in_box(slot, PackedVec::inline_raw(in_box_slots(slot), 0, k));
            let packed = &mut (*slot).packed;
            for v in values.take(k) {
                packed.push(PackedValue::pack(v));
            }
        } else {
            let mut v = Vec::with_capacity(len);
            v.extend(values.map(PackedValue::pack));
            self.install_in_box(slot, PackedVec::from(v));
        }
    }

    /// Install in-box buffers at `slot` holding `packed`.
    ///
    /// # Safety
    /// As [`adopt_in_box`](DenseStorage::adopt_in_box).
    #[inline]
    pub(in crate::value) unsafe fn install_in_box(
        &mut self,
        slot: *mut DenseBuffers,
        packed: PackedVec,
    ) {
        self.release();
        slot.write(DenseBuffers {
            packed,
            classic: None,
        });
        self.0 = Some(NonNull::new_unchecked(slot));
    }

    /// Drop the sidecar (freeing it unless it lives in the object's box).
    #[inline]
    fn release(&mut self) {
        if let Some(p) = self.0.take() {
            let p = p.as_ptr();
            unsafe {
                if p.addr() & DENSE_BOXED != 0 {
                    drop(Box::from_raw(p.map_addr(|a| a & !DENSE_BOXED)));
                } else {
                    std::ptr::drop_in_place(p);
                }
            }
        }
    }

    #[inline]
    pub(in crate::value) fn as_deref(&self) -> Option<&DenseBuffers> {
        self.raw().map(|p| unsafe { &*p })
    }
    #[inline]
    pub(in crate::value) fn as_deref_mut(&mut self) -> Option<&mut DenseBuffers> {
        self.raw().map(|p| unsafe { &mut *p })
    }

    pub(in crate::value) fn is_present(&self) -> bool {
        self.0.is_some()
    }
    #[inline]
    fn buffers_mut(&mut self) -> &mut DenseBuffers {
        if self.0.is_none() {
            *self = DenseStorage::from_box(Some(Box::new(EMPTY_BUFFERS)));
        }
        self.as_deref_mut().expect("sidecar installed above")
    }
    #[inline]
    fn classic(&self) -> Option<&Classic> {
        self.as_deref().and_then(|d| d.classic.as_deref())
    }
    #[inline]
    fn classic_opt_mut(&mut self) -> Option<&mut Classic> {
        self.as_deref_mut().and_then(|d| d.classic.as_deref_mut())
    }
    /// The classic storage, allocated (empty, mirror coherent) when absent.
    #[inline]
    pub(in crate::value) fn classic_mut(&mut self) -> &mut Classic {
        self.buffers_mut().classic.get_or_insert_with(Box::default)
    }
    #[inline]
    pub(in crate::value) fn packed_mut(&mut self) -> Option<&mut PackedVec> {
        let dense = self.as_deref_mut()?;
        (!dense.packed.is_none()).then_some(&mut dense.packed)
    }
    #[inline]
    pub(in crate::value) fn packed_vec(&self) -> Option<&PackedVec> {
        let dense = self.as_deref()?;
        (!dense.packed.is_none()).then_some(&dense.packed)
    }
    #[inline]
    pub(in crate::value) fn packed_ref(&self) -> Option<&[PackedValue]> {
        self.packed_vec().map(|p| &**p)
    }
    pub(in crate::value) fn packed_known_plain(&self) -> bool {
        self.packed_vec().is_some_and(PackedVec::known_plain)
    }
    #[inline]
    pub(in crate::value) fn packed_is_some(&self) -> bool {
        self.packed_vec().is_some()
    }
    /// Switch to packed storage `packed`, dropping any classic storage.
    pub(in crate::value) fn install_packed(&mut self, packed: PackedVec) {
        let dense = self.buffers_mut();
        dense.packed = packed;
        dense.classic = None;
    }
    /// Leave packed mode (an empty array taking the classic route).
    pub(in crate::value) fn drop_packed(&mut self) {
        if let Some(d) = self.as_deref_mut() {
            d.packed = PackedVec::NONE;
        }
    }
    #[inline]
    pub(in crate::value) fn len(&self) -> usize {
        self.classic().map_or(0, |c| c.elems.len())
    }
    #[inline]
    pub(in crate::value) fn get(&self, index: usize) -> Option<&u32> {
        self.classic().and_then(|c| c.elems.get(index))
    }
    #[inline]
    pub(in crate::value) fn get_mut(&mut self, index: usize) -> Option<&mut u32> {
        self.classic_opt_mut().and_then(|c| c.elems.get_mut(index))
    }
    pub(in crate::value) fn reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.classic_mut().elems.reserve_exact(additional);
        }
    }
    #[inline]
    pub(in crate::value) fn push(&mut self, value: u32) {
        self.classic_mut().elems.push(value);
    }
    #[inline]
    pub(in crate::value) fn pop(&mut self) -> Option<u32> {
        self.classic_opt_mut().and_then(|c| c.elems.pop())
    }
    pub(in crate::value) fn clear(&mut self) {
        self.release();
    }
    pub(in crate::value) fn clear_elems(&mut self) {
        if let Some(c) = self.classic_opt_mut() {
            c.elems.clear();
            c.mirror.clear();
        }
    }
    pub(in crate::value) fn iter_mut(&mut self) -> std::slice::IterMut<'_, u32> {
        match self.classic_opt_mut() {
            Some(c) => c.elems.iter_mut(),
            None => [].iter_mut(),
        }
    }

    pub(in crate::value) fn mirror_reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.classic_mut().mirror.reserve_exact(additional);
        }
    }
    pub(in crate::value) fn mirror_len(&self) -> usize {
        self.classic().map_or(0, |c| c.mirror.len())
    }
    pub(in crate::value) fn mirror_get(&self, index: usize) -> Option<&f64> {
        self.classic().and_then(|c| c.mirror.get(index))
    }
    pub(in crate::value) fn mirror_get_mut(&mut self, index: usize) -> Option<&mut f64> {
        self.classic_opt_mut().and_then(|c| c.mirror.get_mut(index))
    }
    pub(in crate::value) fn mirror_push(&mut self, value: f64) {
        self.classic_mut().mirror.push(value);
    }
    pub(in crate::value) fn mirror_pop(&mut self) -> Option<f64> {
        self.classic_opt_mut().and_then(|c| c.mirror.pop())
    }
    #[inline]
    pub(in crate::value) fn mirror_flags(&self) -> u8 {
        self.classic().map_or(MIRROR_DEFAULT, |c| c.mirror_flags)
    }
    /// Mutable flags; allocates the classic storage when absent (a caller clearing a vacuous
    /// mirror should use [`mirror_invalidate`](super::Props::mirror_invalidate) instead).
    #[inline]
    pub(in crate::value) fn mirror_flags_mut(&mut self) -> &mut u8 {
        &mut self.classic_mut().mirror_flags
    }
    #[inline]
    pub(in crate::value) fn mirror_holes_mut(&mut self) -> &mut u32 {
        &mut self.classic_mut().mirror_holes
    }
    /// Drop the mirror, if there is one to drop.
    pub(in crate::value) fn mirror_off(&mut self) {
        if let Some(c) = self.classic_opt_mut() {
            if c.mirror_flags & MIRROR_OK != 0 {
                c.mirror_flags = 0;
                c.mirror.clear();
            }
        }
    }
    /// Reset the mirror to the coherent, hole-free, all-i32 state (after the elements went).
    pub(in crate::value) fn mirror_reset(&mut self) {
        if let Some(c) = self.classic_opt_mut() {
            c.mirror_flags = MIRROR_DEFAULT;
            c.mirror_holes = 0;
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
