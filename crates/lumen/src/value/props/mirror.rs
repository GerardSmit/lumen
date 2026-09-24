//! Numeric mirror synchronization and indexed value updates.
use super::{
    f64_exact_i32, Props, MIRROR_ALL_I32, MIRROR_HOLE, MIRROR_NO_HOLES, MIRROR_OK, NO_SLOT,
};
use crate::value::{canonical_index, Value};

impl Props {
    /// Drop the element mirror (a foreign mutable escape or an unmirrorable element).
    #[inline]
    pub(crate) fn mirror_invalidate(&mut self) {
        if let Some(d) = self.elems.as_deref_mut() {
            if d.mirror_flags & MIRROR_OK != 0 {
                d.mirror_flags = 0;
                d.mirror.clear();
            }
        }
    }

    /// Re-mirror element `n` from `entries[slot]` (both already linked via `elems`).
    /// `filled_hole` = position `n` had no element before this (structural — a *data* value
    /// that happens to equal the hole sentinel must not confuse the accounting).
    pub(super) fn mirror_sync(&mut self, n: usize, slot: usize, filled_hole: bool) {
        if self.elems.mirror_flags() & MIRROR_OK == 0 {
            return;
        }
        if self.elems.mirror_len() != self.elems.len() {
            // Lockstep was broken by a path this code doesn't know — fail safe.
            self.mirror_invalidate();
            return;
        }
        let p = &self.entries[slot];
        match p.value() {
            Value::Num(f) if !p.accessor() && p.writable() && f.to_bits() != MIRROR_HOLE => {
                let d = self.elems.buffers_mut();
                if !f64_exact_i32(f) {
                    d.mirror_flags &= !MIRROR_ALL_I32;
                }
                if filled_hole {
                    d.mirror_holes -= 1;
                    if d.mirror_holes == 0 {
                        d.mirror_flags |= MIRROR_NO_HOLES;
                    }
                }
                d.mirror[n] = f;
            }
            _ => self.mirror_invalidate(),
        }
    }

    /// Grow the mirror alongside `elems` with `pads` holes plus one freshly-linked element.
    pub(super) fn mirror_grow(&mut self, pads: usize, slot: usize) {
        if self.elems.mirror_flags() & MIRROR_OK == 0 {
            return;
        }
        // Peek the value first: an object-element array (its very first push, typically) must
        // not pay a buffer allocation just to invalidate it.
        {
            let p = &self.entries[slot];
            let ok = matches!(p.value(), Value::Num(f) if f.to_bits() != MIRROR_HOLE)
                && !p.accessor()
                && p.writable();
            if !ok {
                self.mirror_invalidate();
                return;
            }
        }
        if pads > 0 {
            let d = self.elems.buffers_mut();
            d.mirror_flags &= !MIRROR_NO_HOLES;
            d.mirror_holes += pads as u32;
            d.mirror
                .extend(std::iter::repeat_n(f64::from_bits(MIRROR_HOLE), pads));
        }
        self.elems.mirror_push(0.0);
        let n = self.elems.mirror_len() - 1;
        self.mirror_sync(n, slot, false); // freshly appended: never a pre-existing hole
    }

    /// One-load dense element read: `Some(f)` is the element's Num value; `None` means the
    /// mirror can't answer (off, out of range, or a hole) — fall back to the classic path,
    /// which is always correct.
    #[inline]
    pub(crate) fn mirror_get(&self, n: u32) -> Option<f64> {
        if self.elems.mirror_flags() & MIRROR_OK == 0 {
            return None;
        }
        let f = *self.elems.mirror_get(n as usize)?;
        if f.to_bits() == MIRROR_HOLE {
            return None;
        }
        Some(f)
    }

    /// Overwrite dense element `n`'s value keeping the mirror coherent. `Err` hands the value
    /// back: no such element, or it isn't a plain writable data property — the caller runs the
    /// generic path.
    #[inline]
    pub(crate) fn set_index_value(&mut self, n: u32, v: Value) -> Result<(), Value> {
        if let Some(packed) = self.elems.packed_mut() {
            let Some(p) = packed.get_mut(n as usize) else {
                return Err(v);
            };
            if matches!(p.value(), Value::Empty) || p.accessor() || !p.writable() {
                return Err(v);
            }
            p.set_value(v);
            return Ok(());
        }
        let Some(&slot) = self.elems.get(n as usize) else {
            return Err(v);
        };
        if slot == NO_SLOT {
            return Err(v);
        }
        let p = &mut self.entries[slot as usize];
        if p.accessor() || !p.writable() {
            return Err(v);
        }
        if self.elems.mirror_flags() & MIRROR_OK != 0 {
            match &v {
                Value::Num(f) if f.to_bits() != MIRROR_HOLE => {
                    if !f64_exact_i32(*f) {
                        *self.elems.mirror_flags_mut() &= !MIRROR_ALL_I32;
                    }
                    // Lockstep holds whenever the flag does; guard anyway.
                    match self.elems.mirror_get_mut(n as usize) {
                        Some(m) => *m = *f,
                        None => self.mirror_invalidate(),
                    }
                }
                _ => self.mirror_invalidate(),
            }
        }
        let p = &mut self.entries[slot as usize];
        p.set_value(v);
        Ok(())
    }

    /// Record a fresh entry at `slot` in the dense map when its key is a canonical index at (or
    /// within a small pad of) the dense frontier. Far-past-the-frontier keys stay name-only.
    pub(super) fn note_inserted(&mut self, slot: usize, key: &str) {
        if !key.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            return;
        }
        if let Some(n) = canonical_index(key) {
            self.note_index_inserted(slot, n);
        }
    }

    /// [`note_inserted`] for a key already known to be canonical index `n`.
    pub(super) fn note_index_inserted(&mut self, slot: usize, n: u32) {
        if slot >= NO_SLOT as usize {
            return;
        }
        let n = n as usize;
        if n < self.elems.len() {
            let filled_hole = self.elems[n] == NO_SLOT;
            self.elems[n] = slot as u32;
            self.mirror_sync(n, slot, filled_hole);
        } else if n <= self.elems.len() + 256 {
            // The pad tolerates *descending* first-fills (`while (--i >= 0) a[i] = 0`,
            // `r[i+n] = x[i]` from the top — bignum/matrix code does this constantly): the
            // first write lands well past the frontier, and a too-small pad would leave the
            // whole upper range map-only for the array's lifetime, killing every dense fast
            // path. 256 covers real dense workloads; a truly sparse `a[1e6]` still stays
            // map-only at ≤1KB of hole slots per object.
            let pads = n - self.elems.len();
            while self.elems.len() < n {
                self.elems.push(NO_SLOT);
            }
            self.elems.push(slot as u32);
            self.mirror_grow(pads, slot);
        } else {
            self.has_far.set(true);
        }
    }
}
