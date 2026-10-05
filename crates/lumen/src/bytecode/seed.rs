//! Parameter seeding shared by prepared VM calls and direct native calls.

use super::Chunk;
use crate::value::Value;

/// Parameter values for a compiled call: at most `cap` are retained.
pub(crate) struct Seed {
    pub(super) p: *mut Value,
    pub(super) n: usize,
    pub(super) cap: usize,
}

impl Seed {
    #[inline(always)]
    pub(crate) fn push(&mut self, value: Value) {
        if self.n < self.cap {
            // SAFETY: `p[..cap]` is reserved for the frame's parameter slots.
            unsafe { self.p.add(self.n).write(value) };
            self.n += 1;
        } else {
            super::drop_value_fast(value);
        }
    }

    #[inline(always)]
    pub(crate) fn wants(&self) -> bool {
        self.n < self.cap
    }
}

/// Run `seed` into `p` and return the number of parameter values written.
///
/// # Safety
/// `p` has room for `min(n_params, n_slots)` values.
pub(crate) unsafe fn seed_raw(p: *mut Value, chunk: &Chunk, seed: impl FnOnce(&mut Seed)) -> usize {
    let mut state = Seed {
        p,
        n: 0,
        cap: chunk.n_params.min(chunk.n_slots),
    };
    seed(&mut state);
    state.n
}
