//! Shared packed-array construction from owned values.
use super::shapes::{array_length_shape, Shape};
use super::storage::{DenseBuffers, DenseStorage};
use super::{EntryVec, PackedVec, Props};
use crate::value::{Property, Value};
use std::cell::Cell;
use std::rc::Rc;

impl Props {
    /// Construct a packed array map with a boxed sidecar and its `length` entry (the
    /// standalone form of what `Object::new_array_*` builds in place).
    #[cfg(test)]
    pub(crate) fn packed_array_from_values(values: impl ExactSizeIterator<Item = Value>) -> Props {
        let len = values.len();
        let mut props = Props::array_shell();
        if len != 0 {
            let mut packed = Vec::with_capacity(len);
            packed.extend(values.map(crate::value::PackedValue::pack));
            assert_eq!(packed.len(), len, "incorrect exact iterator length");
            props.elems.install_packed(PackedVec::from(packed));
        }
        props.entries = std::iter::once(array_length_prop(len)).collect();
        props
    }

    /// An Array's map with its `{length}` shape but no entries and no elements yet: the object
    /// constructors push the named entries into the box's inline slots and install the element
    /// sidecar (see [`packed_elements`]).
    pub(in crate::value) fn array_shell() -> Props {
        Props::array_map(array_length_shape(), None)
    }

    /// A new array box's complete map, built by value inside the box write: the `{length}`
    /// shape `shape`, its `length` entry already written at `named` (the box's inline slots,
    /// `cap` of them) and the element sidecar `elems` (none for an empty array).
    ///
    /// # Safety
    /// As [`EntryVec::inline_raw`] with one initialized entry.
    #[inline(always)]
    pub(in crate::value) unsafe fn array_inline_raw(
        shape: Rc<Shape>,
        named: *mut Property,
        cap: usize,
        elems: DenseStorage,
    ) -> Props {
        Props {
            entries: EntryVec::inline_raw(named, 1, cap),
            shape: shape.id,
            shape_rc: Some(shape),
            elems,
            proto_flag: Cell::new(false),
            has_far: Cell::new(false),
            elem_mode: Cell::new(true),
            ctor_capacity: Cell::new(0),
        }
    }

    /// The `RegExp.prototype.exec` match array's map shell: the `{length, index, input,
    /// groups}` shape, entries left to the caller as for [`Props::array_shell`].
    pub(in crate::value) fn exec_result_shell() -> Props {
        Props::array_map(super::shapes::exec_result_shape(), None)
    }

    fn array_map(shape: Rc<Shape>, sidecar: Option<Box<DenseBuffers>>) -> Props {
        Props {
            entries: EntryVec::new(),
            shape: shape.id,
            shape_rc: Some(shape),
            elems: DenseStorage::from_box(sidecar),
            proto_flag: Cell::new(false),
            has_far: Cell::new(false),
            elem_mode: Cell::new(true),
            ctor_capacity: Cell::new(0),
        }
    }

    /// Append `values` as the dense elements `n..` of an array under construction, in one
    /// reservation. Only when `n` is the dense frontier of a packed (or still elementless)
    /// array map with no far keys — then the whole run lands and `Ok(count)` is returned;
    /// otherwise nothing is consumed and the caller appends one by one.
    pub(crate) fn try_extend_elements<I>(&mut self, n: u32, values: I) -> Result<usize, I>
    where
        I: Iterator<Item = Value>,
    {
        if self.has_far.get() || !self.elem_mode.get() {
            return Err(values);
        }
        let frontier = match self.elems.packed_ref() {
            Some(p) => p.len(),
            None if self.elementless() => 0,
            None => return Err(values),
        };
        if n as usize != frontier {
            return Err(values);
        }
        self.note_structural();
        if !self.elems.packed_is_some() {
            self.install_empty_packed();
        }
        let packed = self
            .elems
            .packed_mut()
            .expect("packed storage installed above");
        let before = packed.len();
        packed.reserve(values.size_hint().0);
        packed.extend(values.map(crate::value::PackedValue::pack));
        Ok(packed.len() - before)
    }

    /// No element storage at all: no sidecar, empty packed storage, or an empty classic one,
    /// and no element-region entries.
    pub(super) fn elementless(&self) -> bool {
        self.elems.packed_ref().is_none_or(|p| p.is_empty())
            && self.elems.len() == 0
            && self.elems.mirror_len() == 0
            && self.entries.len() == self.named_len()
    }

    /// Switch an [`elementless`](Props::elementless) array map to (empty) packed storage.
    pub(super) fn install_empty_packed(&mut self) {
        self.elems.install_packed(PackedVec::default());
    }

    /// A fresh array's empty packed storage gives way to the classic slot map when its first
    /// element lands past index 0 (a descending fill), as an elementless array's does: holes
    /// there are mirror pads, not packed `Empty` slots every fill then has to go generic for.
    /// Returns whether it did.
    pub(super) fn unpack_for_sparse_fill(&mut self, n: u32) -> bool {
        if n == 0 || !self.elems.packed_ref().is_some_and(|p| p.is_empty()) || !self.elementless() {
            return false;
        }
        self.elems.drop_packed();
        true
    }
}

#[cfg(test)]
fn array_length_prop(len: usize) -> Property {
    Property::data(Value::Num(len as f64), true, false, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;

    #[test]
    fn boundaries_preserve_inline_heap_shape_and_length_descriptors() {
        let mut shape = None;
        for len in [0, 1, 10, 11, 32] {
            let props = Props::packed_array_from_values((0..len).map(|n| Value::Num(n as f64)));
            assert_eq!(
                props.elems.packed_ref().map(<[_]>::len),
                (len != 0).then_some(len)
            );
            let length = props.get("length").unwrap();
            assert!(length.writable() && !length.enumerable() && !length.configurable());
            assert!(matches!(length.value(), Value::Num(n) if n == len as f64));
            assert_eq!(props.entries.len(), 1);
            assert!(props.elems.as_deref().is_none_or(|d| d.classic.is_none()));
            assert_eq!(*shape.get_or_insert(props.shape), props.shape);
            for index in 0..len {
                assert!(
                    matches!(props.get_index(index as u32).unwrap().value(), Value::Num(n) if n == index as f64)
                );
            }
            assert!(props.get_index(len as u32).is_none());
        }
    }

    #[test]
    fn owned_builder_transfers_duplicate_owners_once() {
        for len in [2, 10, 11, 32] {
            let child = Object::new(None);
            let initial = crate::value::Gc::strong_count(&child);
            let values: Vec<_> = (0..len).map(|_| Value::Obj(child.clone())).collect();
            let props = Props::packed_array_from_values(values.into_iter());
            assert_eq!(crate::value::Gc::strong_count(&child), initial + len);
            drop(props);
            assert_eq!(crate::value::Gc::strong_count(&child), initial);
        }
    }

    #[test]
    fn iterator_panic_releases_initialized_inline_and_heap_owners() {
        for len in [10, 11] {
            let child = Object::new(None);
            let initial = crate::value::Gc::strong_count(&child);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Props::packed_array_from_values((0..len).map(|index| {
                    assert!(index != 3, "deliberate iterator panic");
                    Value::Obj(child.clone())
                }))
            }));
            assert!(result.is_err());
            assert_eq!(crate::value::Gc::strong_count(&child), initial);
        }
    }

    #[test]
    fn holes_undefined_and_long_runs_keep_ownership() {
        let props = Props::packed_array_from_values([Value::Empty, Value::Undefined].into_iter());
        assert!(props.get_index(0).is_none());
        assert!(matches!(
            props.get_index(1).unwrap().value(),
            Value::Undefined
        ));
        let child = Object::new(None);
        let values = vec![Value::Obj(child.clone()); 33];
        let initial = crate::value::Gc::strong_count(&child) - 33;
        // Any length packs (past the inline capacity: one boxed run).
        let props = Props::packed_array_from_values(values.into_iter());
        assert_eq!(crate::value::Gc::strong_count(&child), initial + 33);
        assert!(props.get_index(32).is_some() && props.get_index(33).is_none());
        drop(props);
        assert_eq!(crate::value::Gc::strong_count(&child), initial);
    }
}
