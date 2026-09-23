//! Shared bounded packed-array construction from owned values.
use super::shapes::array_length_shape;
use super::storage::{DenseBuffers, DenseStorage, InlinePacked, INLINE_PACKED_CAPACITY};
use super::Props;
use crate::value::{Property, Value};
use std::cell::Cell;

impl Props {
    /// Construct at most 32 elements without per-index keys or incremental insertion.
    pub(crate) fn packed_array_from_values(values: impl ExactSizeIterator<Item = Value>) -> Props {
        let len = values.len();
        assert!(len <= 32);
        let inline = len <= INLINE_PACKED_CAPACITY;
        let (inline_packed, packed) = if inline {
            let inline = InlinePacked::from_values(values);
            assert_eq!(
                inline.as_slice().len(),
                len,
                "incorrect exact iterator length"
            );
            (inline, None)
        } else {
            let packed: Vec<_> = values.map(Property::plain).collect();
            assert_eq!(packed.len(), len, "incorrect exact iterator length");
            (InlinePacked::default(), Some(Box::new(packed)))
        };
        let shape = array_length_shape();
        Props {
            entries: std::iter::once(Property::data(Value::Num(len as f64), true, false, false))
                .collect(),
            shape: shape.id,
            shape_rc: Some(shape),
            elems: DenseStorage(Some(Box::new(DenseBuffers {
                packed,
                inline_packed,
                elems: Vec::new(),
                mirror: Vec::new(),
                mirror_flags: 0,
                mirror_holes: 0,
            }))),
            proto_flag: Cell::new(false),
            has_far: Cell::new(false),
            elem_mode: Cell::new(true),
        }
    }
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
            let buffers = props.elems.0.as_ref().unwrap();
            assert_eq!(buffers.packed.is_none(), len <= INLINE_PACKED_CAPACITY);
            assert_eq!(
                buffers.inline_packed.as_slice().len(),
                if len <= 10 { len } else { 0 }
            );
            let length = props.get("length").unwrap();
            assert!(length.writable() && !length.enumerable() && !length.configurable());
            assert!(matches!(length.value(), Value::Num(n) if n == len as f64));
            assert_eq!(props.entries.len(), 1);
            assert_eq!(props.elems.mirror_flags(), 0);
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
    fn holes_undefined_and_rejected_oversize_keep_ownership() {
        let props = Props::packed_array_from_values([Value::Empty, Value::Undefined].into_iter());
        assert!(props.get_index(0).is_none());
        assert!(matches!(
            props.get_index(1).unwrap().value(),
            Value::Undefined
        ));
        let child = Object::new(None);
        let values = vec![Value::Obj(child.clone()); 33];
        let initial = crate::value::Gc::strong_count(&child) - 33;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Props::packed_array_from_values(values.into_iter())
        }))
        .is_err());
        assert_eq!(crate::value::Gc::strong_count(&child), initial);
    }
}
