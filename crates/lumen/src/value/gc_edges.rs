//! Heap edges for cycle collection, without materializing non-object property values.
use super::{Callable, Gc, Property, Value, PACK_EMPTY, PACK_OBJ};

/// Release the source borrow before the collector follows edges, including self references.
pub(crate) fn object_refs_into(object: &Gc, refs: &mut Vec<Gc>) {
    refs.clear();
    let object = object.borrow();
    if let Some(proto) = &object.proto {
        refs.push(proto.clone());
    }
    for property in object.props.values() {
        property.append_object_refs(refs);
    }
    match &object.call {
        Callable::Bound(bound) => {
            refs.push(bound.target.clone());
            if let Value::Obj(object) = &bound.this {
                refs.push(object.clone());
            }
            for argument in &bound.args {
                if let Value::Obj(object) = argument {
                    refs.push(object.clone());
                }
            }
        }
        // A promise's result and pending reactions are heap edges (its suspended coroutine, if
        // any, is not traced: what it holds counts as external, i.e. roots).
        Callable::Promise(slot) => slot.object_refs(refs),
        // The pair shares one cell: only the resolve function reports its edge, so the count
        // never exceeds the promise's real reference count (a lone reject function leaves the
        // promise looking externally held, which is conservative).
        Callable::Resolver(cell, true) => {
            if let Value::Obj(object) = &cell.promise {
                refs.push(object.clone());
            }
        }
        _ => {}
    }
}

impl Property {
    pub(super) fn is_empty(&self) -> bool {
        self.packed.tag() == PACK_EMPTY
    }

    fn append_object_refs(&self, refs: &mut Vec<Gc>) {
        if self.packed.tag() == PACK_OBJ {
            // PACK_OBJ is installed only by packing an owned Gc. Clone that owner while
            // retaining the property's ownership, exactly as PackedValue::unpack does.
            refs.push(unsafe { self.packed.clone_word::<Gc>() });
        }
        if let Some(Value::Obj(object)) = self.getter() {
            refs.push(object.clone());
        }
        if let Some(Value::Obj(object)) = self.setter() {
            refs.push(object.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;
    use std::rc::Rc;

    #[test]
    fn scalar_payloads_have_no_edges_and_only_empty_is_a_hole() {
        let values = [
            Value::Undefined,
            Value::Empty,
            Value::Null,
            Value::Bool(true),
            Value::Num(f64::NAN),
            Value::Num(f64::NEG_INFINITY),
            Value::BigInt(42i64.into()),
            Value::Str("text".into()),
            Value::Sym(Rc::new(crate::value::SymbolData {
                id: 123,
                description: None,
            })),
        ];
        for value in values {
            let empty = matches!(value, Value::Empty);
            let property = Property::plain(value);
            let mut refs = Vec::new();
            property.append_object_refs(&mut refs);
            assert!(refs.is_empty());
            assert_eq!(property.is_empty(), empty);
        }
    }

    #[test]
    fn counts_accessor_storage_even_without_accessor_flag() {
        let target = Object::new(None);
        let mut property = Property::plain(Value::Obj(target.clone()));
        property.set_getter(Some(Value::Obj(target.clone())));
        property.set_setter(Some(Value::Obj(target.clone())));
        assert!(!property.accessor());
        let before = Gc::strong_count(&target);
        let mut refs = Vec::new();
        property.append_object_refs(&mut refs);
        assert_eq!(refs.len(), 3);
        assert!(refs.iter().all(|edge| Gc::ptr_eq(edge, &target)));
        assert_eq!(Gc::strong_count(&target), before + 3);
        refs.clear();
        assert_eq!(Gc::strong_count(&target), before);
    }

    #[test]
    fn preserves_duplicate_edges_and_releases_source_borrow() {
        let object = Object::new(None);
        let target = Object::new(None);
        {
            let mut source = object.borrow_mut();
            source.proto = Some(target.clone());
            source.props.insert(
                "self",
                Property::data(Value::Obj(object.clone()), true, true, true),
            );
            source.props.insert(
                "data",
                Property::data(Value::Obj(target.clone()), true, true, true),
            );
            source.props.insert(
                "accessor",
                Property::accessor_prop(
                    Some(Value::Obj(target.clone())),
                    Some(Value::Obj(target.clone())),
                    true,
                    true,
                ),
            );
            source.props.insert(
                "text",
                Property::data(Value::Str("ignored".into()), true, true, true),
            );
            source.props.insert(
                "number",
                Property::data(Value::Num(f64::NAN), true, true, true),
            );
        }
        let before = Gc::strong_count(&target);
        let mut refs = vec![target.clone()];
        object_refs_into(&object, &mut refs);
        assert_eq!(refs.len(), 5);
        assert_eq!(
            refs.iter()
                .filter(|value| Gc::ptr_eq(value, &target))
                .count(),
            4
        );
        assert_eq!(Gc::strong_count(&target), before + 4);
        for value in &refs {
            drop(value.borrow_mut());
        }
        refs.clear();
        assert_eq!(Gc::strong_count(&target), before);
        object.borrow_mut().props.remove("self");
    }
}
