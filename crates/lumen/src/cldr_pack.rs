//! The packed form of the generated CLDR tables (`cldr_dates`, `cldr_likely`, `cldr_units`;
//! written by `scripts/cldr_pack.py`): every distinct string once, in a sorted pool, and rows of
//! `u16` pool ids. A table of `&str` tuples costs 16 bytes and a base relocation per field —
//! about ten times the text — so this form is what keeps the locale data small.

/// The sorted, deduplicated strings of a table: `text` is their concatenation, `ends[i]` the
/// end offset of string `i` (its start is `ends[i - 1]`, or 0).
pub(crate) struct Pool {
    pub text: &'static str,
    pub ends: &'static [u32],
}

impl Pool {
    /// String `id`.
    pub fn get(&self, id: u16) -> &'static str {
        let i = id as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.text[start..self.ends[i] as usize]
    }

    /// The id of `s`, if the table has it (binary search: the pool is sorted).
    pub fn id(&self, s: &str) -> Option<u16> {
        let (mut lo, mut hi) = (0usize, self.ends.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.get(mid as u16).cmp(s) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u16),
            }
        }
        None
    }
}
