//! Dense element storage and array length access.

use super::{Props, MIRROR_HOLE, MIRROR_NO_HOLES, MIRROR_OK, NO_SLOT};
use crate::value::{Property, Value};

impl Props {
    /// Reserve the exact backing storage for a dense array whose initial length is known.
    /// `entries` needs one additional slot for the array's own `length` property. Small literals
    /// use the keyless packed representation: it avoids allocating/cloning one decimal string key
    /// per element, while all indexed/reflection paths already understand packed properties.
    /// Larger numeric arrays retain the raw-f64 mirror.
    pub(crate) fn reserve_dense_exact(&mut self, len: usize, numeric: bool) {
        if (1..=32).contains(&len) {
            self.entries.reserve_exact(1); // own `length`
            self.elems
                .set_packed(Some(Box::new(super::PackedVec::with_capacity(len))));
            *self.elems.mirror_flags_mut() = 0;
        } else {
            self.entries.reserve_exact(len.saturating_add(1));
            self.elems.reserve_exact(len);
        }
        if numeric && !self.elems.packed_is_some() {
            self.elems.mirror_reserve_exact(len);
        }
    }

    /// Install `elems` as the packed dense elements `0..elems.len()` of an array map that has
    /// its own `length` and no elements yet (a split view materializing in place: see
    /// `crate::split_view`). The same storage a compact builtin array of that length gets.
    pub(crate) fn adopt_packed_elements(&mut self, elems: Vec<Property>) {
        debug_assert!(self.elem_mode.get() && self.elems.packed_ref().is_none_or(|p| p.is_empty()) && self.elems.len() == 0);
        if elems.is_empty() {
            return;
        }
        self.elems
            .set_packed(Some(Box::new(super::PackedVec::from(elems))));
        *self.elems.mirror_flags_mut() = 0;
    }

    /// Mark this map as an array's (see `elem_mode`). One-way, set when the owning object
    /// becomes `Exotic::Array`.
    #[inline]
    pub(crate) fn mark_array(&self) {
        self.elem_mode.set(true);
    }

    /// The `"length"` property, resolved through the shape's `len_slot` memo (one compare, no
    /// hashing). `None` when there is no own `length`.
    pub(crate) fn length_property(&self) -> Option<&Property> {
        let slot = self.find("length")?;
        Some(&self.entries[slot])
    }

    /// Append `v` at index `length` of a plain array whose own `length` is a writable data
    /// property holding a whole number below 2^32 - 1, and bump `length`: the storage half of
    /// `push(v)` (the caller has proved extensibility and that no prototype has elements).
    /// Hands `v` back when this array isn't that simple.
    #[inline]
    pub(crate) fn push_array_element(&mut self, v: Value) -> Result<f64, Value> {
        let Some(slot) = self.find("length") else { return Err(v) };
        let lp = &self.entries[slot];
        let len = match lp.num_value() {
            // `as u32` saturates (NaN and negatives to 0): the round trip proves a whole
            // number in range without a libm `trunc` call.
            Some(n) if lp.writable() && (n as u32) as f64 == n && n < u32::MAX as f64 => n as u32,
            _ => return Err(v),
        };
        let packed = match self.elems.as_deref_mut().and_then(|d| d.packed.as_deref_mut()) {
            Some(p) if p.len() == len as usize && !self.has_far.get() && self.elem_mode.get() => p,
            _ => {
                if let Err(p) = self.try_append_element(len, Property::plain(v)) {
                    return Err(p.into_value());
                }
                let n = len as f64 + 1.0;
                self.entries[slot].set_num_over_num(n);
                return Ok(n);
            }
        };
        packed.push(Property::plain(v));
        self.note_structural();
        let n = len as f64 + 1.0;
        self.entries[slot].set_num_over_num(n);
        Ok(n)
    }

    /// The own property for canonical index `n`, without hashing. `None` only means "not in the
    /// dense map" — the caller must fall back to the string-keyed path, not conclude absence.
    #[inline]
    pub(crate) fn get_index(&self, n: u32) -> Option<&Property> {
        if let Some(packed) = self.elems.packed_ref() {
            return packed
                .get(n as usize)
                .filter(|p| !matches!(p.value(), Value::Empty));
        }
        let slot = *self.elems.get(n as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        Some(&self.entries[slot as usize])
    }

    /// Dense tail append: insert element `n` when `n` is exactly the dense frontier and no
    /// map-only ("far") canonical key exists — which together prove the key is absent, so the
    /// whole existence scan and key-string hashing of [`Props::insert`] can be skipped. Array
    /// (`elem_mode`) maps only: the shape is untouched. Returns `false` (nothing changed) when
    /// the gates don't hold; the caller runs the generic path.
    #[inline]
    pub(crate) fn try_append_element(&mut self, n: u32, prop: Property) -> Result<(), Property> {
        if let Some(packed) = self.elems.packed_ref() {
            if self.has_far.get() || !self.elem_mode.get() || n as usize != packed.len() {
                return Err(prop);
            }
            self.note_structural();
            self.elems.packed_mut().unwrap().push(prop);
            return Ok(());
        }
        if self.has_far.get() || !self.elem_mode.get() || n as usize != self.elems.len() {
            return Err(prop);
        }
        if n == 0 && self.elementless() {
            // The first element of an elementless array: start packed storage (one growable
            // buffer; no per-index slot map or mirror bookkeeping on every later append).
            self.note_structural();
            self.install_empty_packed();
            self.elems.packed_mut().unwrap().push(prop);
            return Ok(());
        }
        self.note_structural();
        let slot = self.entries.len();
        self.reserve_entry();
        self.entries.push(prop);
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
        Ok(())
    }

    /// Insert an absent canonical index directly into the classic dense map, including a bounded
    /// run of holes. The caller has already proved ordinary Array prototype semantics. This is
    /// the numeric-key counterpart of `insert`: it avoids parsing/comparing a decimal key we
    /// already know.
    pub(crate) fn try_define_dense_element(
        &mut self,
        n: u32,
        prop: Property,
    ) -> Result<(), Property> {
        if self.elems.packed_is_some() || self.has_far.get() || !self.elem_mode.get() {
            return Err(prop);
        }
        let n = n as usize;
        let old_len = self.elems.len();
        if n < old_len {
            if self.elems[n] != NO_SLOT {
                return Err(prop);
            }
        } else if n > old_len + 256 {
            return Err(prop);
        }
        self.note_structural();
        let slot = self.entries.len();
        self.reserve_entry();
        self.entries.push(prop);
        if n < old_len {
            self.elems[n] = slot as u32;
            self.mirror_sync(n, slot, true);
        } else {
            let pads = n - old_len;
            while self.elems.len() < n {
                self.elems.push(NO_SLOT);
            }
            self.elems.push(slot as u32);
            self.mirror_grow(pads, slot);
        }
        Ok(())
    }

    /// Copy the run of own plain data elements `start..end` (stopping at the first hole,
    /// accessor or index outside the dense storage) onto `out`, returning how many were copied.
    /// A caller that proved element reads unobservable replaces that many per-index
    /// HasProperty+Get steps with one pass (packed storage is one slice walk).
    pub(crate) fn copy_dense_run(&self, start: u32, end: u32, out: &mut Vec<Value>) -> u32 {
        if start >= end {
            return 0;
        }
        if let Some(packed) = self.elems.packed_ref() {
            let hi = (end as usize).min(packed.len());
            let Some(run) = packed.get(start as usize..hi) else {
                return 0;
            };
            // The run ends at the first accessor or hole; one exact-size extend copies it
            // (each value one full-width store, see `push_value`).
            let n = run
                .iter()
                .position(|p| p.accessor() || p.is_empty())
                .unwrap_or(run.len());
            out.extend(run[..n].iter().map(Property::value));
            return n as u32;
        }
        let mut k = start;
        while k < end {
            match self.get_index(k) {
                Some(p) if !p.accessor() => crate::value::push_value(out, p.value()),
                _ => break,
            }
            k += 1;
        }
        k - start
    }

    /// A copy of packed elements `start..end` when every one of them is a plain data element
    /// (no holes or accessors): each property is cloned as it stands (a refcount bump at most).
    pub(in crate::value) fn clone_packed_run(&self, start: usize, end: usize) -> Option<super::PackedVec> {
        if let Some(pv) = self.elems.packed_boxed() {
            if start <= end && end <= pv.len() && pv.all_flat() {
                return Some(pv.copy_flat(start, end));
            }
        }
        let run = self.elems.packed_ref()?.get(start..end)?;
        if !self.elems.packed_known_plain() && !run.iter().all(Property::is_plain_element) {
            return None;
        }
        // Plain data properties: the clone is the value's (no accessor box to share).
        let copy: Vec<Property> = run.iter().map(Property::clone_plain).collect();
        Some(super::PackedVec::from_plain(copy))
    }

    /// The packed element slice, when this map uses packed storage.
    #[inline]
    pub(crate) fn packed_elements(&self) -> Option<&[Property]> {
        self.elems.packed_ref()
    }

    pub(crate) fn append_element(&mut self, n: u32, prop: Property) -> bool {
        self.try_append_element(n, prop).is_ok()
    }

    /// Dense tail pop: remove element `n` (the array's last) when it is also the last *entry*
    /// (the common stack discipline — elements are appended last) and the last dense slot, and
    /// no "far" canonical key exists. Everything is O(1) pops: no entry shift, no re-index, no
    /// shape change (`elem_mode` maps keep their shape — element keys aren't part of it).
    /// `Some(value)` = removed; `None` = gates failed, nothing changed, caller goes generic.
    pub(crate) fn pop_last_element(&mut self, n: u32) -> Option<Value> {
        if self.has_far.get() || !self.elem_mode.get() {
            return None;
        }
        if let Some(packed) = self.elems.packed_ref() {
            if n as usize + 1 != packed.len() {
                return None;
            }
            let p = packed.last()?;
            if matches!(p.value(), Value::Empty) || p.accessor() || !p.configurable() {
                return None;
            }
            self.note_structural();
            return self
                .elems
                .packed_mut()
                .unwrap()
                .pop()
                .map(Property::into_value);
        }
        if n as usize + 1 != self.elems.len() {
            return None;
        }
        let slot = self.elems[n as usize];
        // The element must be the last entry and sit in the element region (a named index key —
        // `has_far` is excluded above, but a non-array-built map could hold one — would need a
        // shape change).
        if slot == NO_SLOT
            || slot as usize + 1 != self.entries.len()
            || (slot as usize) < self.named_len()
        {
            return None;
        }
        let p = &self.entries[slot as usize];
        if p.accessor() || !p.configurable() {
            return None;
        }
        self.note_structural();
        let p = self.entries.pop().unwrap();
        self.elems.pop();
        if self.elems.mirror_flags() & MIRROR_OK != 0 {
            debug_assert_eq!(self.elems.mirror_len(), self.elems.len() + 1);
            let m = self.elems.mirror_pop();
            if m.map(f64::to_bits) == Some(MIRROR_HOLE) {
                // (Unreachable while the slot was live, but keep the accounting exact.)
                let holes = self.elems.mirror_holes_mut();
                *holes -= 1;
                if *holes == 0 {
                    *self.elems.mirror_flags_mut() |= MIRROR_NO_HOLES;
                }
            }
        }
        Some(p.into_value())
    }

    /// `shift` on a packed array of `len` plain elements (no holes, accessors or read-only
    /// slots) and no far keys: the spec's move-every-element loop leaves exactly the tail's
    /// values in plain slots, so dropping the first element is the whole effect. O(1).
    /// `None` = gates failed, nothing changed.
    pub(crate) fn shift_packed(&mut self, len: u32) -> Option<Value> {
        if self.has_far.get() || !self.elem_mode.get() || len == 0 {
            return None;
        }
        if self.elems.packed_ref()?.len() != len as usize {
            return None;
        }
        let packed = self.elems.packed_mut()?;
        if !packed.all_plain() {
            return None;
        }
        self.note_structural();
        self.elems.packed_mut()?.pop_front().map(Property::into_value)
    }

    /// `unshift(...items)` on a packed array of `len` plain elements with no far keys. The
    /// caller has proved the new indices can't reach a prototype setter and that the array is
    /// extensible. O(items) amortized.
    pub(crate) fn unshift_packed(&mut self, len: u32, items: &[Value]) -> bool {
        if self.has_far.get() || !self.elem_mode.get() {
            return false;
        }
        match self.elems.packed_ref() {
            Some(p) if p.len() == len as usize => {}
            None if len == 0 && self.elementless() => {
                self.note_structural();
                self.install_empty_packed();
            }
            _ => return false,
        }
        let Some(packed) = self.elems.packed_mut() else { return false };
        if !packed.all_plain() {
            return false;
        }
        self.note_structural();
        self.elems
            .packed_mut()
            .unwrap()
            .prepend(items.iter().cloned().map(Property::plain));
        true
    }

    /// `splice(start, del, ...items)` on a packed array of `len` plain elements with no far
    /// keys; returns the removed values. The caller has proved (when the array grows) that new
    /// indices can't reach a prototype setter and that the array is extensible.
    pub(crate) fn splice_packed(
        &mut self,
        len: u32,
        start: usize,
        del: usize,
        items: &[Value],
    ) -> Option<Vec<Value>> {
        if self.has_far.get() || !self.elem_mode.get() || start + del > len as usize {
            return None;
        }
        if self.elems.packed_ref()?.len() != len as usize || !self.elems.packed_mut()?.all_plain() {
            return None;
        }
        self.note_structural();
        let removed = self.elems.packed_mut()?.splice(
            start,
            del,
            items.iter().cloned().map(Property::plain),
        );
        Some(removed.into_iter().map(Property::into_value).collect())
    }

    /// Append the next dense element while *building a fresh array in order*: element index ==
    /// dense slot, entry slot == the next free entry. Skips the canonical-index parse and any
    /// key allocation. Only valid on an array map whose dense elements so far are exactly
    /// 0..len.
    pub(crate) fn push_dense(&mut self, prop: Property) {
        if let Some(packed) = self.elems.packed_mut() {
            packed.push(prop);
            return;
        }
        let slot = self.entries.len();
        self.reserve_entry();
        self.entries.push(prop);
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
    }
}
