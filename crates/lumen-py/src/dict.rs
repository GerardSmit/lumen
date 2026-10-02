//! Insertion-ordered hash table keyed by Python values. Hashing and equality that may run user
//! code live in the interpreter; this table only does the native fast comparisons and hands back
//! the candidates that need a full `__eq__`.

use crate::object::{Kind, Value};
use std::rc::Rc;

#[derive(Clone)]
pub struct Entry {
    pub hash: i64,
    pub key: Value,
    pub val: Value,
}

pub enum Lookup {
    Found(usize),
    Missing,
    Slow(Vec<(usize, Value)>),
}

const EMPTY: u32 = u32::MAX;
const DUMMY: u32 = u32::MAX - 1;
const LINEAR_MAX: usize = 8;

#[derive(Clone, Default)]
pub struct PyDict {
    entries: Vec<Option<Entry>>,
    indices: Vec<u32>,
    live: usize,
    set_mode: bool,
    dummy: Vec<bool>,
    fill: usize,
}

const SET_MIN: usize = 8;
const SET_LINEAR: usize = 9;

/// Native equality for keys whose comparison cannot run user code; `None` means "ask the VM".
pub fn fast_eq(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x == y),
        (Value::None, Value::None) => Some(true),
        (Value::Bool(x), Value::Bool(y)) => Some(x == y),
        (Value::Bool(x), Value::Int(y)) | (Value::Int(y), Value::Bool(x)) => Some(*x as i64 == *y),
        (Value::Float(x), Value::Float(y)) => Some(x == y || (x.is_nan() && x.to_bits() == y.to_bits())),
        (Value::Float(x), Value::Int(y)) | (Value::Int(y), Value::Float(x)) => {
            Some(*x == *y as f64 && (*y as f64) as i64 == *y)
        }
        (Value::Float(x), Value::Bool(y)) | (Value::Bool(y), Value::Float(x)) => Some(*x == *y as i64 as f64),
        (Value::Obj(x), Value::Obj(y)) => {
            if Rc::ptr_eq(x, y) {
                return Some(true);
            }
            if x.cls.is_some() || y.cls.is_some() {
                return None;
            }
            match (&x.kind, &y.kind) {
                (Kind::Str(s), Kind::Str(t)) => Some(s.s == t.s),
                (Kind::Tuple(s), Kind::Tuple(t)) => {
                    if s.len() != t.len() {
                        return Some(false);
                    }
                    for (p, q) in s.iter().zip(t.iter()) {
                        match fast_eq(p, q) {
                            Some(true) => {}
                            Some(false) => return Some(false),
                            None => return None,
                        }
                    }
                    Some(true)
                }
                (Kind::Int(s), Kind::Int(t)) => Some(s == t),
                (Kind::Bytes(s), Kind::Bytes(t)) => Some(s == t),
                (Kind::Str(_) | Kind::Tuple(_) | Kind::Bytes(_), Kind::Str(_) | Kind::Tuple(_) | Kind::Bytes(_)) => Some(false),
                _ => None,
            }
        }
        (Value::Obj(o), Value::Int(_) | Value::Bool(_) | Value::None | Value::Float(_))
        | (Value::Int(_) | Value::Bool(_) | Value::None | Value::Float(_), Value::Obj(o)) => {
            let plain = o.cls.is_none();
            let int_vs_int = matches!(&o.kind, Kind::Int(_)) && !matches!(a, Value::Float(_) | Value::None) && !matches!(b, Value::Float(_) | Value::None);
            if plain && (matches!(&o.kind, Kind::Str(_) | Kind::Tuple(_) | Kind::Bytes(_)) || int_vs_int) {
                Some(false)
            } else {
                None
            }
        }
        (Value::None, _) | (_, Value::None) => Some(false),
        _ => None,
    }
}

fn probe_next(slot: usize, perturb: &mut u64, mask: usize) -> usize {
    *perturb >>= 5;
    (slot.wrapping_mul(5).wrapping_add(*perturb as usize).wrapping_add(1)) & mask
}

impl PyDict {
    pub fn new() -> PyDict {
        PyDict::default()
    }

    /// A table laid out like CPython's set (open addressing with linear probes), so iteration
    /// order matches for hashes that are deterministic (ints, floats, tuples of those).
    pub fn new_set() -> PyDict {
        PyDict { set_mode: true, ..PyDict::default() }
    }

    fn set_slot_probe(&self, hash: i64, mut visit: impl FnMut(usize) -> bool) {
        let mask = self.entries.len() - 1;
        let mut i = (hash as u64 as usize) & mask;
        let mut perturb = hash as u64;
        loop {
            let probes = if i + SET_LINEAR <= mask { SET_LINEAR } else { 0 };
            for k in 0..=probes {
                if visit(i + k) {
                    return;
                }
            }
            perturb >>= 5;
            i = i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize) & mask;
        }
    }

    fn set_lookup(&self, hash: i64, key: &Value) -> Lookup {
        if self.entries.is_empty() {
            return Lookup::Missing;
        }
        let mut found = None;
        let mut slow: Vec<(usize, Value)> = Vec::new();
        self.set_slot_probe(hash, |s| match &self.entries[s] {
            None => !self.dummy[s],
            Some(e) => {
                if e.hash == hash {
                    match fast_eq(&e.key, key) {
                        Some(true) => {
                            found = Some(s);
                            return true;
                        }
                        Some(false) => {}
                        None => slow.push((s, e.key.clone())),
                    }
                }
                false
            }
        });
        if let Some(s) = found {
            Lookup::Found(s)
        } else if slow.is_empty() {
            Lookup::Missing
        } else {
            Lookup::Slow(slow)
        }
    }

    fn set_insert_clean(entries: &mut [Option<Entry>], e: Entry) {
        let mask = entries.len() - 1;
        let mut i = (e.hash as u64 as usize) & mask;
        let mut perturb = e.hash as u64;
        loop {
            let probes = if i + SET_LINEAR <= mask { SET_LINEAR } else { 0 };
            for k in 0..=probes {
                if entries[i + k].is_none() {
                    entries[i + k] = Some(e);
                    return;
                }
            }
            perturb >>= 5;
            i = i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize) & mask;
        }
    }

    fn set_resize(&mut self, minused: usize) {
        let mut newsize = SET_MIN;
        while newsize <= minused {
            newsize <<= 1;
        }
        let old = std::mem::take(&mut self.entries);
        let mut entries: Vec<Option<Entry>> = vec![None; newsize];
        for e in old.into_iter().flatten() {
            Self::set_insert_clean(&mut entries, e);
        }
        self.entries = entries;
        self.dummy = vec![false; newsize];
        self.fill = self.live;
    }

    fn set_insert(&mut self, hash: i64, key: Value, val: Value) -> usize {
        if self.entries.is_empty() {
            self.entries = vec![None; SET_MIN];
            self.dummy = vec![false; SET_MIN];
        }
        let mut free: Option<usize> = None;
        let mut unused = 0usize;
        self.set_slot_probe(hash, |s| {
            if self.entries[s].is_none() {
                if self.dummy[s] {
                    if free.is_none() {
                        free = Some(s);
                    }
                    false
                } else {
                    unused = s;
                    true
                }
            } else {
                false
            }
        });
        let e = Entry { hash, key, val };
        if let Some(s) = free {
            self.entries[s] = Some(e);
            self.dummy[s] = false;
            self.live += 1;
            return s;
        }
        self.entries[unused] = Some(e);
        self.live += 1;
        self.fill += 1;
        let mask = self.entries.len() - 1;
        if self.fill * 5 >= mask * 3 {
            let target = if self.live > 50000 { self.live * 2 } else { self.live * 4 };
            self.set_resize(target);
            return self.set_slot_of(hash);
        }
        unused
    }

    fn set_slot_of(&self, hash: i64) -> usize {
        let mut out = 0;
        self.set_slot_probe(hash, |s| {
            if matches!(&self.entries[s], Some(e) if e.hash == hash) {
                out = s;
                true
            } else {
                self.entries[s].is_none()
            }
        });
        out
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, idx: usize) -> Option<&Entry> {
        self.entries.get(idx).and_then(|e| e.as_ref())
    }

    pub fn set_val(&mut self, idx: usize, val: Value) {
        if let Some(Some(e)) = self.entries.get_mut(idx) {
            e.val = val;
        }
    }

    pub fn next_live(&self, mut pos: usize) -> Option<usize> {
        while pos < self.entries.len() {
            if self.entries[pos].is_some() {
                return Some(pos);
            }
            pos += 1;
        }
        None
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.entries.iter().flatten()
    }

    pub fn keys(&self) -> Vec<Value> {
        self.iter().map(|e| e.key.clone()).collect()
    }

    pub fn values(&self) -> Vec<Value> {
        self.iter().map(|e| e.val.clone()).collect()
    }

    pub fn lookup(&self, hash: i64, key: &Value) -> Lookup {
        if self.set_mode {
            return self.set_lookup(hash, key);
        }
        let mut slow: Vec<(usize, Value)> = Vec::new();
        if self.indices.is_empty() {
            for (i, e) in self.entries.iter().enumerate() {
                if let Some(e) = e {
                    if e.hash == hash {
                        match fast_eq(&e.key, key) {
                            Some(true) => return Lookup::Found(i),
                            Some(false) => {}
                            None => slow.push((i, e.key.clone())),
                        }
                    }
                }
            }
        } else {
            let mask = self.indices.len() - 1;
            let mut slot = (hash as u64 as usize) & mask;
            let mut perturb = hash as u64;
            loop {
                let ix = self.indices[slot];
                if ix == EMPTY {
                    break;
                }
                if ix != DUMMY {
                    if let Some(e) = &self.entries[ix as usize] {
                        if e.hash == hash {
                            match fast_eq(&e.key, key) {
                                Some(true) => return Lookup::Found(ix as usize),
                                Some(false) => {}
                                None => slow.push((ix as usize, e.key.clone())),
                            }
                        }
                    }
                }
                slot = probe_next(slot, &mut perturb, mask);
            }
        }
        if slow.is_empty() {
            Lookup::Missing
        } else {
            Lookup::Slow(slow)
        }
    }

    pub fn find_str(&self, hash: i64, s: &str) -> Option<usize> {
        let matches = |e: &Entry| -> bool {
            e.hash == hash
                && match &e.key {
                    Value::Obj(o) => matches!(&o.kind, Kind::Str(p) if &*p.s == s),
                    _ => false,
                }
        };
        if self.indices.is_empty() {
            for (i, e) in self.entries.iter().enumerate() {
                if let Some(e) = e {
                    if matches(e) {
                        return Some(i);
                    }
                }
            }
            None
        } else {
            let mask = self.indices.len() - 1;
            let mut slot = (hash as u64 as usize) & mask;
            let mut perturb = hash as u64;
            loop {
                let ix = self.indices[slot];
                if ix == EMPTY {
                    return None;
                }
                if ix != DUMMY {
                    if let Some(e) = &self.entries[ix as usize] {
                        if matches(e) {
                            return Some(ix as usize);
                        }
                    }
                }
                slot = probe_next(slot, &mut perturb, mask);
            }
        }
    }

    fn place(indices: &mut [u32], hash: i64, idx: u32) {
        let mask = indices.len() - 1;
        let mut slot = (hash as u64 as usize) & mask;
        let mut perturb = hash as u64;
        while indices[slot] != EMPTY && indices[slot] != DUMMY {
            slot = probe_next(slot, &mut perturb, mask);
        }
        indices[slot] = idx;
    }

    fn rebuild(&mut self, extra: usize) {
        self.entries.retain(|e| e.is_some());
        let n = self.entries.len() + extra;
        if n > LINEAR_MAX {
            let mut cap = 16;
            while cap < n * 3 {
                cap *= 2;
            }
            let mut indices = vec![EMPTY; cap];
            for (i, e) in self.entries.iter().enumerate() {
                if let Some(e) = e {
                    Self::place(&mut indices, e.hash, i as u32);
                }
            }
            self.indices = indices;
        } else {
            self.indices = Vec::new();
        }
    }

    /// Appends a key known to be absent.
    pub fn insert_new(&mut self, hash: i64, key: Value, val: Value) -> usize {
        if self.set_mode {
            return self.set_insert(hash, key, val);
        }
        let n = self.entries.len() + 1;
        if n > LINEAR_MAX && (self.indices.is_empty() || n * 3 > self.indices.len() * 2) {
            self.rebuild(1);
        }
        let idx = self.entries.len();
        self.entries.push(Some(Entry { hash, key, val }));
        if !self.indices.is_empty() {
            Self::place(&mut self.indices, hash, idx as u32);
        }
        self.live += 1;
        idx
    }

    pub fn remove(&mut self, idx: usize) -> Option<Entry> {
        if self.set_mode {
            let e = self.entries.get_mut(idx)?.take()?;
            self.dummy[idx] = true;
            self.live -= 1;
            return Some(e);
        }
        let e = self.entries.get_mut(idx)?.take()?;
        self.live -= 1;
        if !self.indices.is_empty() {
            let mask = self.indices.len() - 1;
            let mut slot = (e.hash as u64 as usize) & mask;
            let mut perturb = e.hash as u64;
            loop {
                if self.indices[slot] == idx as u32 {
                    self.indices[slot] = DUMMY;
                    break;
                }
                if self.indices[slot] == EMPTY {
                    break;
                }
                slot = probe_next(slot, &mut perturb, mask);
            }
        }
        if self.live == 0 {
            self.entries.clear();
            self.indices.clear();
        }
        Some(e)
    }

    /// Empties the table and returns its former contents (so they can be released later).
    pub fn take_all(&mut self) -> PyDict {
        let fresh = PyDict { set_mode: self.set_mode, ..PyDict::default() };
        std::mem::replace(self, fresh)
    }

    pub fn clear(&mut self) {
        self.dummy.clear();
        self.fill = 0;
        self.entries.clear();
        self.indices.clear();
        self.live = 0;
    }

    /// Mirrors CPython's `set_merge`: presizing and, when the target is empty, reinserting in the
    /// source's table order.
    pub fn merge_set(&mut self, other: &PyDict) {
        if other.live == 0 {
            return;
        }
        if self.entries.is_empty() {
            self.entries = vec![None; SET_MIN];
            self.dummy = vec![false; SET_MIN];
        }
        let mask = self.entries.len() - 1;
        if (self.fill + other.live) * 5 >= mask * 3 {
            self.set_resize((self.live + other.live) * 2);
        }
        if self.fill == 0 {
            if self.entries.len() == other.entries.len() && other.fill == other.live {
                self.entries = other.entries.clone();
                self.dummy = vec![false; self.entries.len()];
            } else {
                for e in other.entries.iter().flatten() {
                    Self::set_insert_clean(&mut self.entries, e.clone());
                }
            }
            self.live = other.live;
            self.fill = other.live;
            return;
        }
        for e in other.entries.iter().flatten() {
            if matches!(self.set_lookup(e.hash, &e.key), Lookup::Found(_)) {
                continue;
            }
            self.set_insert(e.hash, e.key.clone(), e.val.clone());
        }
    }

    pub fn last_live(&self) -> Option<usize> {
        (0..self.entries.len()).rev().find(|&i| self.entries[i].is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_set(items: &[i64]) -> PyDict {
        let mut d = PyDict::new_set();
        for &i in items {
            if let Lookup::Missing = d.lookup(i, &Value::Int(i)) {
                d.insert_new(i, Value::Int(i), Value::None);
            }
        }
        d
    }

    fn order(d: &PyDict) -> Vec<i64> {
        d.iter()
            .map(|e| match e.key {
                Value::Int(i) => i,
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn set_iterates_small_ints_in_slot_order() {
        assert_eq!(order(&int_set(&[3, 1, 2])), vec![1, 2, 3]);
        assert_eq!(order(&int_set(&[5, 4, 3, 2, 1])), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn set_collisions_wrap_with_the_table_mask() {
        assert_eq!(order(&int_set(&[8, 1, 16])), vec![8, 1, 16]);
        assert_eq!(order(&int_set(&[16, 8, 1])), vec![16, 8, 1]);
    }

    #[test]
    fn set_resize_keeps_every_element_reachable() {
        let items: Vec<i64> = (0..200).map(|i| i * 37 % 1009).collect();
        let d = int_set(&items);
        assert_eq!(d.len(), 200);
        for &i in &items {
            assert!(matches!(d.lookup(i, &Value::Int(i)), Lookup::Found(_)));
        }
    }

    #[test]
    fn set_removal_leaves_probe_chains_intact() {
        let mut d = int_set(&[0, 8, 16, 24]);
        let Lookup::Found(slot) = d.lookup(8, &Value::Int(8)) else { panic!("missing") };
        assert!(d.remove(slot).is_some());
        assert!(matches!(d.lookup(8, &Value::Int(8)), Lookup::Missing));
        assert!(matches!(d.lookup(24, &Value::Int(24)), Lookup::Found(_)));
        assert_eq!(d.len(), 3);
        d.insert_new(8, Value::Int(8), Value::None);
        assert_eq!(d.len(), 4);
    }

    #[test]
    fn dict_keeps_insertion_order_across_deletes() {
        let mut d = PyDict::new();
        for i in 0..40 {
            d.insert_new(i, Value::Int(i), Value::Int(i * 2));
        }
        for i in (0..40).step_by(2) {
            if let Lookup::Found(ix) = d.lookup(i, &Value::Int(i)) {
                d.remove(ix);
            }
        }
        assert_eq!(order(&d), (0..40).filter(|i| i % 2 == 1).collect::<Vec<_>>());
    }
}
