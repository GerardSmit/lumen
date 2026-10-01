//! Heap edges for cycle collection, visited in place: no handle is cloned and no property value
//! is materialized.
use super::{Callable, Gc, Object, Property, Props, Value};
use std::mem::ManuallyDrop;

/// Visit the edges an object holds outside its property map: its prototype and the objects its
/// call behavior references.
pub(crate) fn visit_object_head(object: &Object, f: &mut impl FnMut(&Gc)) {
    if let Some(proto) = &object.proto {
        f(proto);
    }
    match &object.call {
        Callable::Bound(bound) => {
            f(&bound.target);
            if let Value::Obj(object) = &bound.this {
                f(object);
            }
            for argument in &bound.args {
                if let Value::Obj(object) = argument {
                    f(object);
                }
            }
        }
        // A promise's result and pending reactions are heap edges (its suspended coroutine, if
        // any, is not traced: what it holds counts as external, i.e. roots).
        Callable::Promise(slot) => slot.visit_object_refs(f),
        // The pair shares one cell: only the resolve function reports its edge, so the count
        // never exceeds the promise's real reference count (a lone reject function leaves the
        // promise looking externally held, which is conservative).
        Callable::Resolver(cell, true) => {
            if let Value::Obj(object) = &cell.promise {
                f(object);
            }
        }
        _ => {}
    }
}

/// Visit every edge of `object`. The caller must not mutate it meanwhile.
pub(crate) fn visit_object_refs(object: &Object, f: &mut impl FnMut(&Gc)) {
    visit_object_head(object, f);
    object.props.visit_object_refs(0, usize::MAX, f);
}

impl Props {
    /// Visit the objects held by property slots `from..from + limit`, in the order `values`
    /// yields them (packed elements, then entries). Returns where to resume, or `None` once
    /// every slot has been visited, so a huge array is walked in bounded steps.
    pub(crate) fn visit_object_refs(
        &self,
        from: usize,
        limit: usize,
        f: &mut impl FnMut(&Gc),
    ) -> Option<usize> {
        let packed = self.elems.packed_ref().unwrap_or(&[]);
        let entries = &self.entries[..];
        let total = packed.len() + entries.len();
        let end = total.min(from.saturating_add(limit));
        for i in from..end {
            if let Some(i) = i.checked_sub(packed.len()) {
                entries[i].visit_object_refs(f);
            } else if let Some(p) = packed[i].obj_ptr() {
                // SAFETY: a packed object word is a live handle; it is only borrowed.
                f(&ManuallyDrop::new(unsafe { Gc::from_raw(p) }));
            }
        }
        (end < total).then_some(end)
    }
}

impl Property {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.packed.tag() == super::PACK_EMPTY
    }

    pub(crate) fn visit_object_refs(&self, f: &mut impl FnMut(&Gc)) {
        if let Some(p) = self.packed.obj_ptr() {
            // SAFETY: a packed object word is a live handle; it is only borrowed.
            f(&ManuallyDrop::new(unsafe { Gc::from_raw(p) }));
        }
        if let Some(Value::Obj(object)) = self.getter() {
            f(object);
        }
        if let Some(Value::Obj(object)) = self.setter() {
            f(object);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;
    use std::rc::Rc;

    fn property_refs(property: &Property) -> Vec<Gc> {
        let mut refs = Vec::new();
        property.visit_object_refs(&mut |g| refs.push(g.clone()));
        refs
    }

    fn object_refs(object: &Gc) -> Vec<Gc> {
        let mut refs = Vec::new();
        visit_object_refs(&object.borrow(), &mut |g| refs.push(g.clone()));
        refs
    }

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
            assert!(property_refs(&property).is_empty());
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
        let mut refs = property_refs(&property);
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
        let mut refs = object_refs(&object);
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
