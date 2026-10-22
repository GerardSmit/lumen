//! Compile-time open-addressing index over a fixed list of names, for O(1)
//! name to position lookups in `no_std` code without a lazily built map.

use crate::fasthash::{fnv1a64, FNV1A64_OFFSET};

/// `N` must be a power of two of at least twice the number of names (and at
/// most 65536). Entries hold `position + 1`; zero is empty. When a name
/// repeats, the first position wins, matching a linear `position` scan.
pub struct NameTable<const N: usize> {
    entries: [u16; N],
}

const fn bytes_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn bucket(name: &[u8], mask: usize) -> usize {
    (fnv1a64(FNV1A64_OFFSET, name) >> 20) as usize & mask
}

impl<const N: usize> NameTable<N> {
    pub const fn build(names: &[&str]) -> Self {
        assert!(N.is_power_of_two() && N <= 1 << 16 && names.len() * 2 <= N);
        let mut entries = [0u16; N];
        let mask = N - 1;
        let mut position = 0;
        while position < names.len() {
            let name = names[position].as_bytes();
            let mut slot = bucket(name, mask);
            loop {
                let held = entries[slot];
                if held == 0 {
                    entries[slot] = (position + 1) as u16;
                    break;
                }
                if bytes_equal(names[held as usize - 1].as_bytes(), name) {
                    break;
                }
                slot = (slot + 1) & mask;
            }
            position += 1;
        }
        Self { entries }
    }

    /// Position of `name` in `names`, which must be the list the table was built from.
    pub fn find(&self, names: &[&str], name: &str) -> Option<usize> {
        let mask = N - 1;
        let mut slot = bucket(name.as_bytes(), mask);
        loop {
            let held = self.entries[slot];
            if held == 0 {
                return None;
            }
            if names[held as usize - 1] == name {
                return Some(held as usize - 1);
            }
            slot = (slot + 1) & mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 4] = ["color", "width", "color", "height"];
    static TABLE: NameTable<16> = NameTable::build(&NAMES);

    #[test]
    fn finds_first_position_and_misses_unknown() {
        assert_eq!(TABLE.find(&NAMES, "color"), Some(0));
        assert_eq!(TABLE.find(&NAMES, "height"), Some(3));
        assert_eq!(TABLE.find(&NAMES, "depth"), None);
    }
}
