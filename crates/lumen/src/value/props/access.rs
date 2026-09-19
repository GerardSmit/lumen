//! Named lookup, slot access, and ordered reflection.
use super::shapes::index_key;
use super::{Props, NO_SLOT};
use crate::value::{canonical_index, Property, Value};
use std::rc::Rc;

impl Props {
    /// The named-entry slot for `key`: a scan of the shape's key list for small shapes (≤
    /// [`super::shapes::INDEX_THRESHOLD`] keys — most objects), else the shape's shared hash
    /// index. Never finds an element-region entry — those are keyed by the dense sidecar (see
    /// [`Props::slot_of`]).
    #[inline(always)]
    pub(super) fn find(&self, key: &str) -> Option<usize> {
        self.shape_rc.as_ref()?.find(key).map(|s| s as usize)
    }

    /// The classic dense-map slot of canonical index `n` (packed storage has no slots).
    #[inline]
    fn element_slot(&self, n: u32) -> Option<usize> {
        if self.elems.packed_is_some() {
            return None;
        }
        let slot = *self.elems.get(n as usize)?;
        (slot != NO_SLOT).then_some(slot as usize)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Property> {
        if let Some(n) = canonical_index(key) {
            if let Some(p) = self.get_index(n) {
                return Some(p);
            }
            // Every canonical index is recorded in the dense sidecar unless a deliberately
            // sparse, far-ahead insertion has ever occurred. With no such insertion, a dense
            // miss proves absence; scanning the shape's keys is both redundant and especially
            // costly for a hole read from a large array.
            if !self.has_far.get() {
                return None;
            }
        }
        self.find(key).map(|i| &self.entries[i])
    }

    /// The memoized own `prototype` slot, for guarded constructor fast paths.
    #[inline]
    pub(crate) fn prototype_slot(&self) -> Option<u32> {
        self.find("prototype").map(|slot| slot as u32)
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut Property> {
        if let Some(n) = canonical_index(key) {
            if self
                .elems
                .packed_ref()
                .and_then(|p| p.get(n as usize))
                .is_some_and(|p| !matches!(p.value(), Value::Empty))
            {
                return self.elems.packed_mut().and_then(|p| p.get_mut(n as usize));
            }
        }
        if key.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        match self.slot_of(key) {
            Some(i) => Some(&mut self.entries[i]),
            None => None,
        }
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The `entries` slot for `key` (named or classic element), or `None`. Backs the bytecode
    /// property inline cache: a hit records the slot so the next access can skip the lookup (see
    /// `Interp::try_ic_get`); only named slots (`< named_len`) are ever recorded.
    #[inline]
    pub(crate) fn slot_of(&self, key: &str) -> Option<usize> {
        if let Some(n) = canonical_index(key) {
            if let Some(slot) = self.element_slot(n) {
                return Some(slot);
            }
            if !self.has_far.get() {
                return None;
            }
        }
        self.find(key)
    }

    /// The property at `slot`, or `None` if out of range. A cached slot is only trusted after
    /// the shape matched — the named prefix of `entries` is pinned by the shape.
    #[inline]
    pub(crate) fn entry_at(&self, slot: usize) -> Option<&Property> {
        self.entries.get(slot)
    }

    /// Mutable [`entry_at`], for the property write inline cache.
    #[inline]
    pub(crate) fn entry_at_mut(&mut self, slot: usize) -> Option<&mut Property> {
        // A mirror lives in the sidecar: without one there is nothing to invalidate, and the
        // key check (a walk of a shared shape's chain) is skipped for every plain object.
        if self.elems.is_present()
            && (slot >= self.named_len()
                || self.shape_rc.as_ref().is_some_and(|s| {
                    s.key_at(slot)
                        .as_bytes()
                        .first()
                        .is_some_and(|b| b.is_ascii_digit())
                }))
        {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        self.entries.get_mut(slot)
    }

    /// Canonical indices of the element-region entries (slots past the named prefix), ascending,
    /// with their slots.
    pub(super) fn element_region(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let named = self.named_len();
        let has_region = self.entries.len() > named;
        (0..if has_region { self.elems.len() } else { 0 }).filter_map(move |n| {
            let slot = *self.elems.get(n)? as usize;
            (slot != NO_SLOT as usize && slot >= named).then_some((n, slot))
        })
    }

    /// Keys in insertion order for named keys; packed and element-region keys ascending first.
    /// Private-name slots (`#x`) are never enumerable/observable, so they are excluded here (and
    /// from [`ordered_keys`]); private access reads them via [`get`] directly.
    pub(crate) fn keys(&self) -> Vec<Rc<str>> {
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter().enumerate())
            .filter(|(_, p)| !matches!(p.value(), Value::Empty))
            .map(|(n, _)| index_key(n))
            .chain(self.element_region().map(|(n, _)| index_key(n)))
            .chain(self.shape_keys().iter().cloned())
            .filter(|k| !crate::interpreter::Interp::is_private_key(k))
            .collect()
    }

    /// Keys in spec [[OwnPropertyKeys]] order: array-index keys ascending, then other string keys
    /// in insertion order, then symbol keys in insertion order.
    pub(crate) fn ordered_keys(&self) -> Vec<Rc<str>> {
        let mut ints: Vec<(u32, Rc<str>)> = Vec::new();
        let mut strs: Vec<Rc<str>> = Vec::new();
        let mut syms: Vec<Rc<str>> = Vec::new();
        if let Some(packed) = self.elems.packed_ref() {
            ints.extend(
                packed
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !matches!(p.value(), Value::Empty))
                    .map(|(n, _)| (n as u32, index_key(n))),
            );
        }
        ints.extend(self.element_region().map(|(n, _)| (n as u32, index_key(n))));
        let mut named_ints = false;
        for k in self.shape_keys() {
            if crate::interpreter::Interp::is_private_key(k) {
                continue; // private-element slot — not an observable own key
            }
            if crate::interpreter::Interp::is_sym_key(k) {
                syms.push(k.clone());
            } else if let Some(n) = canonical_index(k) {
                ints.push((n, k.clone()));
                named_ints = true;
            } else {
                strs.push(k.clone());
            }
        }
        if named_ints {
            ints.sort_by_key(|(n, _)| *n);
        }
        ints.into_iter()
            .map(|(_, k)| k)
            .chain(strs)
            .chain(syms)
            .collect()
    }

    /// Every keyed entry — named keys in slot order, then element-region entries ascending.
    /// Packed elements have no entries and are not visited (see [`keys`] / [`values`]).
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Rc<str>, &Property)> {
        self.shape_keys()
            .iter()
            .cloned()
            .zip(self.entries.iter())
            .chain(
                self.element_region()
                    .map(|(n, slot)| (index_key(n), &self.entries[slot])),
            )
    }

    /// Named (shape-keyed) entries only, in slot order, borrowing the keys.
    pub(crate) fn iter_named(&self) -> impl Iterator<Item = (&Rc<str>, &Property)> {
        self.shape_keys().iter().zip(self.entries.iter())
    }

    /// Every live property value, including keyless packed elements (for GC tracing).
    pub(crate) fn values(&self) -> impl Iterator<Item = &Property> {
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter())
            .filter(|p| !p.is_empty())
            .chain(self.entries.iter())
    }

    pub(crate) fn highest_nonconfig_index_from(&self, from: usize) -> Option<usize> {
        let packed = self
            .elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter().enumerate())
            .filter_map(|(n, p)| {
                (!matches!(p.value(), Value::Empty) && !p.configurable() && n >= from).then_some(n)
            });
        let region = self
            .element_region()
            .filter(|&(n, slot)| n >= from && !self.entries[slot].configurable())
            .map(|(n, _)| n);
        let named = self.iter_named().filter_map(|(k, p)| {
            (!p.configurable())
                .then(|| canonical_index(k).map(|n| n as usize))
                .flatten()
                .filter(|&n| n >= from)
        });
        packed.chain(region).chain(named).max()
    }

    pub(crate) fn integrity_ok(&self, frozen: bool) -> bool {
        let valid = |p: &Property| !p.configurable() && (!frozen || p.accessor() || !p.writable());
        let named = self.named_len();
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter())
            .filter(|p| !matches!(p.value(), Value::Empty))
            .all(valid)
            && self.entries[named..].iter().all(valid)
            && self
                .iter_named()
                .all(|(k, p)| crate::interpreter::Interp::is_private_key(k) || valid(p))
    }
}
