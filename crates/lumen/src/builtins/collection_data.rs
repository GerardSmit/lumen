//! Insertion-ordered collection storage for Map, Set, WeakMap and WeakSet (see `table`).

mod dense;
mod index;
mod table;

use crate::fasthash::FxHasher;
use crate::value::{Gc, PackedValue, Value};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use table::{KeyEntry, PairEntry, Table};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    #[default]
    Map,
    Set,
    WeakMap,
    WeakSet,
}

impl CollectionKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Map => "Map",
            Self::Set => "Set",
            Self::WeakMap => "WeakMap",
            Self::WeakSet => "WeakSet",
        }
    }
}

// A Set entry's key doubles as its value, so Sets store keys only.
enum Store {
    Pairs(Table<PairEntry>),
    Keys(Table<KeyEntry>),
}

macro_rules! table {
    ($store:expr, $table:ident => $body:expr) => {
        match $store {
            Store::Pairs($table) => $body,
            Store::Keys($table) => $body,
        }
    };
}

pub(crate) struct CollectionData {
    kind: CollectionKind,
    store: Store,
}

impl Default for CollectionData {
    fn default() -> Self {
        Self::new(CollectionKind::Map)
    }
}

enum Iter<'a> {
    Pairs(table::Iter<'a, PairEntry>),
    Keys(table::Iter<'a, KeyEntry>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a PackedValue, &'a PackedValue);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Pairs(it) => it.next(),
            Self::Keys(it) => it.next(),
        }
    }
}

impl CollectionData {
    pub(crate) fn new(kind: CollectionKind) -> Self {
        let store = match kind {
            CollectionKind::Set | CollectionKind::WeakSet => Store::Keys(Table::default()),
            CollectionKind::Map | CollectionKind::WeakMap => Store::Pairs(Table::default()),
        };
        Self { kind, store }
    }

    pub(crate) fn kind(&self) -> CollectionKind {
        self.kind
    }

    /// Whether entries hold a value apart from the key (false for Sets).
    pub(crate) fn has_values(&self) -> bool {
        matches!(self.store, Store::Pairs(_))
    }

    pub(crate) fn len(&self) -> usize {
        table!(&self.store, t => t.len())
    }

    /// The next live entry at or after `cursor`, advancing it. Cursors are opaque positions
    /// that stay valid across every mutation of the collection; an iteration starts at 0.
    #[inline]
    pub(crate) fn next(&self, cursor: &mut usize) -> Option<(&PackedValue, &PackedValue)> {
        table!(&self.store, t => t.next(cursor))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&PackedValue, &PackedValue)> {
        match &self.store {
            Store::Pairs(t) => Iter::Pairs(t.iter()),
            Store::Keys(t) => Iter::Keys(t.iter()),
        }
    }

    pub(crate) fn lookup(&self, key: &Value) -> Option<Value> {
        table!(&self.store, t => t.lookup(key).map(PackedValue::unpack))
    }

    pub(crate) fn contains(&self, key: &Value) -> bool {
        table!(&self.store, t => t.contains(key))
    }

    /// A native DOMString-set lookup using the same seeded hash/index as JS Set.
    #[cfg(feature = "embed")]
    pub(crate) fn contains_string(&self,key:&str)->bool {
        let mut hash=FxHasher::default();hash.write(key.as_bytes());
        table!(&self.store,t=>t.contains_string(key,hash.finish() as u32))
    }

    pub(crate) fn insert(&mut self, key: Value, value: Value) {
        let key = match key {
            Value::Num(n) if n == 0.0 => Value::Num(0.0),
            // An empty key would pack to the tombstone marker.
            Value::Empty => Value::Undefined,
            other => other,
        };
        table!(&mut self.store, t => t.insert(key, value))
    }

    pub(crate) fn remove(&mut self, key: &Value) -> bool {
        table!(&mut self.store, t => t.remove(key))
    }

    pub(crate) fn clear(&mut self) {
        table!(&mut self.store, t => t.clear())
    }

    #[cfg(test)]
    fn slots(&self) -> usize {
        table!(&self.store, t => t.slots())
    }

    #[cfg(test)]
    fn next_owned(&self, cursor: &mut usize) -> Option<(Value, Value)> {
        self.next(cursor).map(|(k, v)| (k.unpack(), v.unpack()))
    }

    #[cfg(test)]
    fn owned(&self) -> Vec<(Value, Value)> {
        self.iter().map(|(k, v)| (k.unpack(), v.unpack())).collect()
    }
}

impl FromIterator<(Value, Value)> for CollectionData {
    fn from_iter<T: IntoIterator<Item = (Value, Value)>>(iter: T) -> Self {
        let mut data = Self::default();
        for (key, value) in iter {
            data.insert(key, value);
        }
        data
    }
}

fn key_hash(key: &Value) -> u64 {
    // Seeded (see `fasthash`), so scripts cannot precompute colliding keys. Keys of different
    // types may share a hash input; `same_key` tells them apart.
    let mut hash = FxHasher::default();
    match key {
        Value::Str(v) => hash.write(v.as_str().as_bytes()),
        Value::Num(n) => {
            let bits = if n.is_nan() {
                f64::NAN.to_bits()
            } else if *n == 0.0 {
                0
            } else {
                n.to_bits()
            };
            hash.write_u64(bits);
        }
        Value::Obj(v) => hash.write_usize(Gc::as_ptr(v) as usize),
        Value::Sym(v) => hash.write_usize(Rc::as_ptr(v) as usize),
        Value::Bool(v) => hash.write_u64(*v as u64),
        Value::BigInt(v) => v.hash(&mut hash),
        Value::Undefined | Value::Empty | Value::Null => {}
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::{CollectionData, key_hash};
    use crate::fasthash::tests::{OldFx, old_colliding_keys};
    use crate::value::{Object, Value};
    use std::hash::{Hash, Hasher};
    use std::time::{Duration, Instant};

    fn time_inserts(keys: &[Value]) -> Duration {
        let start = Instant::now();
        let mut data = CollectionData::default();
        for k in keys {
            data.insert(k.clone(), Value::Undefined);
        }
        for k in keys {
            assert!(data.contains(k));
        }
        assert_eq!(data.len(), keys.len());
        start.elapsed()
    }

    #[test]
    fn string_keys_colliding_under_the_old_hash_insert_in_linear_time() {
        // The old key hash began with the discriminant, then `Hash for str`.
        let mut prefix = OldFx::default();
        std::mem::discriminant(&Value::str("")).hash(&mut prefix);
        let hostile: Vec<Value> = old_colliding_keys(&prefix, 20_000)
            .iter()
            .map(|k| Value::str(k.as_str()))
            .collect();
        let old_digest = |k: &Value| {
            let mut h = OldFx::default();
            std::mem::discriminant(k).hash(&mut h);
            let Value::Str(s) = k else { unreachable!() };
            s.as_str().hash(&mut h);
            h.finish()
        };
        assert!(
            hostile
                .iter()
                .all(|k| old_digest(k) == old_digest(&hostile[0]))
        );
        let benign: Vec<Value> = (0..hostile.len())
            .map(|i| Value::str(format!("benign-key-{i:05}").as_str()))
            .collect();
        let (hostile, benign) = (time_inserts(&hostile), time_inserts(&benign));
        assert!(
            hostile < benign * 20 + Duration::from_millis(50),
            "hostile {hostile:?} vs benign {benign:?}"
        );
    }

    #[test]
    fn colliding_keys_remain_distinct_through_updates_and_deletes() {
        // `true` and the smallest denormal both hash the word 1.
        let text = Value::Bool(true);
        let number = Value::Num(f64::from_bits(1));
        assert_eq!(key_hash(&number), key_hash(&text));
        let mut data = CollectionData::default();
        data.insert(text.clone(), Value::Num(1.0));
        data.insert(number.clone(), Value::Num(2.0));
        data.insert(text.clone(), Value::Num(3.0));
        assert_eq!(data.len(), 2);
        assert!(matches!(data.lookup(&text), Some(Value::Num(3.0))));
        assert!(matches!(data.lookup(&number), Some(Value::Num(2.0))));
        assert!(data.remove(&text));
        assert!(!data.contains(&text));
        assert!(data.contains(&number));
        data.insert(text.clone(), Value::Num(4.0));
        assert!(data.remove(&number));
        assert!(matches!(data.lookup(&text), Some(Value::Num(4.0))));
    }

    #[test]
    fn nan_payloads_and_signed_zero_share_entries() {
        let mut data = CollectionData::default();
        data.insert(Value::Num(-0.0), Value::Num(1.0));
        data.insert(Value::Num(0.0), Value::Num(2.0));
        data.insert(Value::Num(f64::NAN), Value::Num(3.0));
        let other_nan = Value::Num(f64::from_bits(0xfff8_0000_0000_0042));
        data.insert(other_nan.clone(), Value::Num(4.0));
        assert_eq!(data.len(), 2);
        assert!(
            matches!(data.owned().into_iter().next(), Some((Value::Num(n), _)) if n.to_bits() == 0)
        );
        assert!(matches!(
            data.lookup(&Value::Num(f64::NAN)),
            Some(Value::Num(4.0))
        ));
        assert!(data.remove(&other_nan));
        assert!(data.remove(&Value::Num(-0.0)));
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn clear_releases_entries_without_invalidating_cursors() {
        let mut data = CollectionData::default();
        let key = Object::new(None);
        let value = Object::new(None);
        data.insert(Value::Obj(key.clone()), Value::Obj(value.clone()));
        let mut cursor = 0;
        assert!(data.next_owned(&mut cursor).is_some());
        data.clear();
        assert_eq!(crate::value::Gc::strong_count(&key), 1);
        assert_eq!(crate::value::Gc::strong_count(&value), 1);
        assert!(data.slots() == 0);
        data.insert(Value::Num(42.0), Value::Undefined);
        assert!(matches!(
            data.next_owned(&mut cursor),
            Some((Value::Num(42.0), _))
        ));
        // An iterator created before clear but never started sees the new entry too.
        assert!(matches!(
            data.next_owned(&mut 0),
            Some((Value::Num(42.0), _))
        ));
    }

    #[test]
    fn delete_releases_references_and_reinsertion_appends() {
        let mut data = CollectionData::default();
        let key = Object::new(None);
        let value = Object::new(None);
        data.insert(Value::Obj(key.clone()), Value::Obj(value.clone()));
        data.insert(Value::Num(1.0), Value::Undefined);
        assert!(data.remove(&Value::Obj(key.clone())));
        assert_eq!(crate::value::Gc::strong_count(&key), 1);
        assert_eq!(crate::value::Gc::strong_count(&value), 1);
        data.insert(Value::Obj(key.clone()), Value::Undefined);
        let mut cursor = 0;
        assert!(matches!(
            data.next_owned(&mut cursor),
            Some((Value::Num(1.0), _))
        ));
        assert!(
            matches!(data.next_owned(&mut cursor), Some((Value::Obj(o), _)) if crate::value::Gc::ptr_eq(&o, &key))
        );
        assert!(data.next_owned(&mut cursor).is_none());
    }
    #[test]
    fn weak_deletion_churn_does_not_accumulate_vacant_slots() {
        let mut data = CollectionData::default();
        let stable = Object::new(None);
        data.insert(Value::Obj(stable.clone()), Value::Num(42.0));
        for _ in 0..1000 {
            let temporary = Value::Obj(Object::new(None));
            data.insert(temporary.clone(), Value::Undefined);
            assert!(data.remove(&temporary));
            assert!(!data.contains(&temporary));
        }
        assert!(data.slots() <= 7);
        assert!(matches!(
            data.lookup(&Value::Obj(stable)),
            Some(Value::Num(42.0))
        ));
    }
}
