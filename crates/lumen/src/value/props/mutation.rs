//! Property insertion, removal, and shape maintenance.
use super::shapes::{
    fresh_owned_id, shape_by_id, shape_owned_from, shape_transition, Shape, OWNED_THRESHOLD,
    SHAPE_EMPTY,
};
use super::{Props, MIRROR_HOLE, MIRROR_NO_HOLES, MIRROR_OK, NO_SLOT};
use crate::value::{canonical_index, Property, Value};
use std::rc::Rc;

impl Props {
    /// Drop every property (used by the GC to break a garbage object's reference cycles).
    pub(crate) fn clear(&mut self) {
        self.note_structural();
        self.entries.clear();
        self.elems.clear();
        self.shape_rc = None;
        self.shape = SHAPE_EMPTY;
    }

    /// Install `shape` as this map's shape.
    #[inline]
    fn set_shape(&mut self, shape: Rc<Shape>) {
        self.shape = shape.id;
        self.shape_rc = Some(shape);
    }

    /// Detach to an owned shape holding the current key list (a structural change that is not
    /// a tree transition follows).
    fn detach_shape(&mut self) -> &mut Shape {
        if !self.shape_rc.as_ref().is_some_and(|s| s.owned()) {
            let owned = shape_owned_from(self.shape_rc.as_deref());
            self.set_shape(owned);
        }
        let shape = self.shape_rc.as_mut().expect("owned shape just installed");
        Rc::get_mut(shape).expect("owned shapes have one owner")
    }

    /// Re-id an owned shape after mutating its key list, and mirror the id here.
    fn reid_owned_shape(&mut self) {
        let id = fresh_owned_id();
        let shape = self.shape_rc.as_mut().expect("owned shape");
        Rc::get_mut(shape).expect("owned shapes have one owner").id = id;
        self.shape = id;
    }

    /// Add `key` to the shape: a tree transition while the shape is shared and small, else an
    /// in-place append on an owned shape (detaching first when the shared chain would grow past
    /// [`OWNED_THRESHOLD`]).
    fn transition_key(&mut self, key: Rc<str>) {
        let shared = self.shape_rc.as_ref().is_none_or(|s| !s.owned());
        if shared && self.named_len() < OWNED_THRESHOLD {
            let child = shape_transition(self.shape_rc.as_ref(), &key);
            self.set_shape(child);
            return;
        }
        self.detach_shape().push_key(key);
        self.reid_owned_shape();
    }

    /// Make room for a new named property at slot `named_len` (the end of the named prefix):
    /// an element sitting there moves to the end of `entries`. Returns the slot.
    fn open_named_slot(&mut self, prop: Property) -> usize {
        let slot = self.named_len();
        self.reserve_entry();
        if slot < self.entries.len() {
            let displaced = std::mem::replace(&mut self.entries[slot], prop);
            self.entries.push(displaced);
            let moved_to = (self.entries.len() - 1) as u32;
            if self.elems.is_present() {
                for e in self.elems.iter_mut() {
                    if *e == slot as u32 {
                        *e = moved_to;
                        break;
                    }
                }
            }
        } else {
            self.entries.push(prop);
        }
        slot
    }

    /// Insert a key *known to be absent* (the caller shape-validated the map), landing on a
    /// *known* child shape: skips both the existence scan and the transition-table lookup that
    /// [`Props::insert`] pays. `new_shape` must be the memoized `shape_transition(shape, key)`
    /// result recorded when this (shape, key) pair was first inserted the slow way — a shared
    /// shape id (see [`Props::shape_is_shared`]).
    pub(crate) fn append_new(&mut self, key: Rc<str>, prop: Property, new_shape: u32) {
        self.note_structural();
        let child = shape_by_id(new_shape);
        debug_assert_eq!(child.last_key().map(|k| &**k), Some(&*key));
        debug_assert_eq!(child.len(), self.named_len() + 1);
        let slot = self.open_named_slot(prop);
        self.set_shape(child);
        self.note_inserted(slot, &key);
    }

    pub(crate) fn insert(&mut self, key: impl Into<Rc<str>>, prop: Property) {
        let key = key.into();
        let index = canonical_index(&key);
        if let (Some(n), Some(packed)) = (index, self.elems.packed_ref()) {
            let n = n as usize;
            if n < packed.len() {
                self.note_structural();
                self.elems.packed_mut().unwrap()[n] = prop;
                return;
            }
            if !self.has_far.get() && n <= packed.len() + 256 {
                self.note_structural();
                let packed = self.elems.packed_mut().unwrap();
                packed.resize_with(n, || Property::plain(Value::Empty));
                packed.push(prop);
                return;
            }
            self.has_far.set(true);
        }
        if let Some(i) = self.slot_of(&key) {
            self.entries[i] = prop;
            if self.elems.mirror_flags() & MIRROR_OK != 0
                && key.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
            {
                match index {
                    Some(n) if (n as usize) < self.elems.mirror_len() => {
                        // Replacing an existing entry: position n already had the element.
                        self.mirror_sync(n as usize, i, false)
                    }
                    // A far/map-only index entry stays outside the mirror's range: fine.
                    Some(_) => {}
                    None => self.mirror_invalidate(), // "007"-style: not canonical, be safe
                }
            }
            return;
        }
        self.note_structural();
        let dense_element = self.elem_mode.get()
            && !self.elems.packed_is_some()
            && index.is_some_and(|n| n as usize <= self.elems.len() + 256);
        let slot = if dense_element {
            // An array element: no shape transition, element region (after the named prefix).
            // Farther-out indices become named keys (`has_far`) so they stay reachable by name.
            self.reserve_entry();
            self.entries.push(prop);
            self.entries.len() - 1
        } else {
            let slot = self.open_named_slot(prop);
            self.transition_key(key.clone());
            slot
        };
        self.note_inserted(slot, &key);
    }

    /// Remove `entries[slot]`, shifting later slots down and repointing the dense map. The
    /// shape is the caller's business.
    fn remove_slot(&mut self, slot: usize) -> Property {
        let removed = self.entries.remove(slot);
        // Dense slots shift down past the removed entry; the removed key's own slot holes.
        if self.elems.is_present() {
            for e in self.elems.iter_mut() {
                if *e == NO_SLOT {
                    continue;
                }
                match (*e as usize).cmp(&slot) {
                    std::cmp::Ordering::Equal => *e = NO_SLOT,
                    std::cmp::Ordering::Greater => *e -= 1,
                    std::cmp::Ordering::Less => {}
                }
            }
        }
        removed
    }

    /// Remove every canonical-index key `>= from` in one pass — array truncation
    /// (`arr.length = n`). Entries compact and the dense map rebuilds once: O(n) total, where
    /// the per-key [`Props::remove`] loop it replaces was O(n) *per key*.
    pub(crate) fn remove_indices_from(&mut self, from: usize) {
        let packed_remove = self.elems.packed_ref().is_some_and(|p| {
            p.len() > from && p[from..].iter().any(|p| !matches!(p.value(), Value::Empty))
        });
        let named = self.named_len();
        // Slot → canonical index for every dense-mapped entry (element region and named index
        // keys alike).
        let mut slot_index: Vec<u32> = vec![NO_SLOT; self.entries.len()];
        if !self.elems.packed_is_some() {
            for n in 0..self.elems.len() {
                let slot = self.elems[n];
                if slot != NO_SLOT {
                    slot_index[slot as usize] = n as u32;
                }
            }
        }
        let keep: Vec<bool> = {
            let keys = self.shape_keys();
            (0..self.entries.len())
                .map(|slot| {
                    if slot < named {
                        !canonical_index(&keys[slot]).is_some_and(|n| n as usize >= from)
                    } else {
                        slot_index[slot] == NO_SLOT || (slot_index[slot] as usize) < from
                    }
                })
                .collect()
        };
        let any_named = keep[..named].iter().any(|k| !k);
        let any_entry = keep.iter().any(|k| !k);
        if !packed_remove && !any_entry {
            return;
        }
        self.note_structural();
        if let Some(packed) = self.elems.packed_mut() {
            packed.truncate(from);
        }
        if !any_entry {
            return;
        }
        let mut slot = 0;
        self.entries.retain(|_| {
            let k = keep[slot];
            slot += 1;
            k
        });
        // Element-region entries need their index back once the dense map is rebuilt.
        let mut kept_indices: Vec<(usize, u32)> = Vec::new();
        let mut new_slot = 0;
        for (old_slot, &k) in keep.iter().enumerate() {
            if k {
                if old_slot >= named && slot_index[old_slot] != NO_SLOT {
                    kept_indices.push((new_slot, slot_index[old_slot]));
                }
                new_slot += 1;
            }
        }
        if any_named {
            // A removal shifts named slots: not a tree transition, so detach to an owned shape.
            let keep_named = &keep[..named];
            self.detach_shape().retain_keys(|slot, _| keep_named[slot]);
            self.reid_owned_shape();
        }
        self.elems.clear_elems();
        self.elems.mirror_reset();
        for slot in 0..self.named_len() {
            let key = self.shape_rc.as_ref().expect("named keys").key_at(slot);
            if let Some(n) = canonical_index(key) {
                self.note_index_inserted(slot, n);
            }
        }
        for (slot, n) in kept_indices {
            self.note_index_inserted(slot, n);
        }
    }

    pub(crate) fn remove(&mut self, key: &str) -> bool {
        let index = canonical_index(key);
        if let (Some(n), Some(packed)) = (index, self.elems.packed_ref()) {
            if packed
                .get(n as usize)
                .is_some_and(|p| !matches!(p.value(), Value::Empty))
            {
                self.note_structural();
                self.elems.packed_mut().unwrap()[n as usize] = Property::plain(Value::Empty);
                return true;
            }
        }
        let Some(i) = self.slot_of(key) else {
            return false;
        };
        self.note_structural();
        if self.elems.mirror_flags() & MIRROR_OK != 0 {
            if let Some(n) = index {
                if (n as usize) < self.elems.mirror_len()
                    && self.elems.mirror_get(n as usize).unwrap().to_bits() != MIRROR_HOLE
                {
                    *self.elems.mirror_get_mut(n as usize).unwrap() = f64::from_bits(MIRROR_HOLE);
                    *self.elems.mirror_flags_mut() &= !MIRROR_NO_HOLES;
                    *self.elems.mirror_holes_mut() += 1;
                }
            }
        }
        let named = i < self.named_len();
        self.remove_slot(i);
        if named {
            // Named slots shifted: not a tree transition, so detach to an owned shape. Element
            // region removals leave the shape alone — array shapes track named keys only.
            self.detach_shape().remove_key(i);
            self.reid_owned_shape();
        }
        true
    }
}
