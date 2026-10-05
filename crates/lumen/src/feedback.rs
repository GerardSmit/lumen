//! Host feedback. File identities depend on source operations, never heap addresses.
use crate::value::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub struct Profile {
    sites: BTreeMap<(u64, u32), BTreeMap<(u32, u32, u64), u64>>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Profile>> = const { RefCell::new(None) };
}

/// Record this host execution, restoring an enclosing collector on unwind.
pub fn record<R>(body: impl FnOnce() -> R) -> (R, Profile) {
    struct Restore(Option<Profile>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|a| *a.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ACTIVE.with(|a| a.replace(Some(Profile::default()))));
    let result = body();
    let profile = ACTIVE.with(|a| a.borrow_mut().take().unwrap());
    (result, profile)
}

pub(crate) fn hash(bytes: &[u8], seed: u64) -> u64 {
    lumen_common::fasthash::fnv1a64(seed, bytes)
}

pub(crate) fn observe(key: impl FnOnce() -> u64, pc: usize, left: &Value, right: &Value) {
    ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let Some(profile) = active.as_mut() else {
            return;
        };
        let shape = crate::native_ops::shape_hash(left);
        let count = profile
            .sites
            .entry((key(), pc as u32))
            .or_default()
            .entry((
                crate::native_ops::value_kind(left),
                crate::native_ops::value_kind(right),
                shape,
            ))
            .or_default();
        *count = count.saturating_add(1);
    });
}

impl Profile {
    pub fn shape(&self, key: u64, pc: usize) -> Option<u64> {
        let kinds = self.sites.get(&(key, pc as u32))?;
        if kinds.len() != 1 {
            return None;
        }
        let &(left, right, shape) = kinds.keys().next()?;
        (left == 8 && right == 0 && shape != 0).then_some(shape)
    }
    pub fn numeric(&self, key: u64, pc: usize) -> bool {
        self.sites.get(&(key, pc as u32)).is_some_and(|kinds| {
            !kinds.is_empty()
                && kinds
                    .keys()
                    .all(|&(left, right, _)| left == 4 && right == 4)
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"LMPGO\0\x01\0".to_vec();
        for (&(key, pc), kinds) in &self.sites {
            for (&(left, right, shape), &count) in kinds {
                out.extend_from_slice(&key.to_le_bytes());
                out.extend_from_slice(&pc.to_le_bytes());
                out.extend_from_slice(&left.to_le_bytes());
                out.extend_from_slice(&right.to_le_bytes());
                out.extend_from_slice(&shape.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 || &bytes[..8] != b"LMPGO\0\x01\0" || (bytes.len() - 8) % 36 != 0 {
            return Err("invalid Lumen profile format".into());
        }
        let mut profile = Self::default();
        for row in bytes[8..].chunks_exact(36) {
            let key = u64::from_le_bytes(row[0..8].try_into().unwrap());
            let pc = u32::from_le_bytes(row[8..12].try_into().unwrap());
            let left = u32::from_le_bytes(row[12..16].try_into().unwrap());
            let right = u32::from_le_bytes(row[16..20].try_into().unwrap());
            let shape = u64::from_le_bytes(row[20..28].try_into().unwrap());
            let count = u64::from_le_bytes(row[28..36].try_into().unwrap());
            if left > 8 || right > 8 || count == 0 {
                return Err("invalid Lumen profile observation".into());
            }
            let observations = profile.sites.entry((key, pc)).or_default();
            if observations.insert((left, right, shape), count).is_some() {
                return Err("duplicate Lumen profile observation".into());
            }
        }
        Ok(profile)
    }
}
