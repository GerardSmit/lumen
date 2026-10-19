//! Host-neutral IndexedDB records and atomic transaction snapshots.
//! Values are the host's structured-clone bytes; this module never interprets
//! a language object or accesses an operating-system storage API.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum Key { Number(f64), Date(f64), String(Vec<u16>), Binary(Vec<u8>), Array(Vec<Key>) }
impl Key {
    pub fn retained_bytes(&self) -> usize { match self {
        Self::Number(_) | Self::Date(_) => 8,
        Self::String(text) => text.len().saturating_mul(2),
        Self::Binary(bytes) => bytes.len(),
        Self::Array(keys) => keys.iter().fold(0usize, |total, key| total.saturating_add(32).saturating_add(key.retained_bytes())),
    } }
    fn rank(&self) -> u8 { match self { Self::Number(_) => 0, Self::Date(_) => 1, Self::String(_) => 2, Self::Binary(_) => 3, Self::Array(_) => 4 } }
    pub fn number(number: f64) -> Result<Self, Error> {
        if number.is_nan() { Err(Error::Data) } else { Ok(Self::Number(if number == 0.0 { 0.0 } else { number })) }
    }
}
impl PartialEq for Key { fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal } }
impl Eq for Key {}
impl PartialOrd for Key { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank()).then_with(|| match (self, other) {
            (Self::Number(a), Self::Number(b)) | (Self::Date(a), Self::Date(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Self::String(a), Self::String(b)) => a.cmp(b), (Self::Binary(a), Self::Binary(b)) => a.cmp(b),
            (Self::Array(a), Self::Array(b)) => a.cmp(b), _ => Ordering::Equal,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRange { pub lower: Option<Key>, pub upper: Option<Key>, pub lower_open: bool, pub upper_open: bool }
impl KeyRange {
    pub fn contains(&self, key: &Key) -> bool {
        self.lower.as_ref().is_none_or(|bound| if self.lower_open { key > bound } else { key >= bound })
            && self.upper.as_ref().is_none_or(|bound| if self.upper_open { key < bound } else { key <= bound })
    }
    pub fn validate(&self) -> Result<(), Error> {
        if let (Some(lower), Some(upper)) = (&self.lower, &self.upper) {
            if lower > upper || (lower == upper && (self.lower_open || self.upper_open)) { return Err(Error::Data); }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error { Data, Constraint, NotFound, InvalidState, InvalidAccess, Syntax, TransactionInactive, ReadOnly, Version, Abort, QuotaExceeded, Conflict }
impl Error {
    pub fn name(self) -> &'static str { match self {
        Self::Data => "DataError", Self::Constraint => "ConstraintError", Self::NotFound => "NotFoundError",
        Self::InvalidState => "InvalidStateError", Self::TransactionInactive => "TransactionInactiveError",
        Self::InvalidAccess => "InvalidAccessError", Self::Syntax => "SyntaxError",
        Self::ReadOnly => "ReadOnlyError", Self::Version => "VersionError", Self::Abort => "AbortError",
        Self::QuotaExceeded => "QuotaExceededError", Self::Conflict => "UnknownError",
    } }
}

#[derive(Clone, Debug)]
pub enum KeyPath { String(String), Sequence(Vec<String>) }
impl KeyPath {
    pub fn validate(&self) -> Result<(), Error> {
        let paths: &[String] = match self { Self::String(path) => std::slice::from_ref(path), Self::Sequence(paths) => paths };
        if paths.is_empty() { return Err(Error::Syntax); }
        if paths.len() > 4096 { return Err(Error::QuotaExceeded); }
        for path in paths {
            if path.len() > 1024 * 1024 { return Err(Error::QuotaExceeded); }
            if path.is_empty() { continue; }
            for segment in path.split('.') {
                let mut chars = segment.chars();
                let property = |name, c: char| crate::unicode_props::lookup(name, None).is_some_and(|ranges| ranges.binary_search_by(|&(start, end)| {
                    if (c as u32) < start { Ordering::Greater } else if (c as u32) > end { Ordering::Less } else { Ordering::Equal }
                }).is_ok());
                let start = |c: char| c == '$' || c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && property("ID_Start", c));
                if !chars.next().is_some_and(start) || !chars.all(|c| start(c) || c.is_ascii_digit() || c == '\u{200C}' || c == '\u{200D}' || (!c.is_ascii() && property("ID_Continue", c))) { return Err(Error::Syntax); }
            }
        }
        Ok(())
    }
    fn bytes(&self) -> usize { match self { Self::String(path) => path.len(), Self::Sequence(paths) => paths.iter().map(|path| 32 + path.len()).sum() } }
}

#[derive(Clone, Debug)]
pub struct Index {
    pub key_path: KeyPath, pub unique: bool, pub multi_entry: bool,
    pub records: Arc<BTreeMap<Key, BTreeSet<Key>>>,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectStore {
    pub key_path: Option<KeyPath>, pub auto_increment: bool, pub generator: u64,
    pub indexes: BTreeMap<String, Index>,
    // Snapshotting a read transaction shares both keys and clone bytes. A
    // writer copies the bounded ordered metadata only on its first mutation.
    pub records: Arc<BTreeMap<Key, Arc<[u8]>>>,
}
#[derive(Clone, Debug, Default)]
pub struct Database { pub version: u64, pub revision: u64, pub stores: BTreeMap<String, ObjectStore> }
fn store_bytes(name: &str, store: &ObjectStore) -> usize {
    128 + name.len() + store.key_path.as_ref().map_or(0, KeyPath::bytes)
        + store.records.iter().map(|(key, value)| 64 + key.retained_bytes() + value.len()).sum::<usize>()
        + store.indexes.iter().map(|(name, index)| 128 + name.len() + index.key_path.bytes() + index.records.iter().map(|(key, primary)| 64 + key.retained_bytes() + primary.iter().map(|key| 32 + key.retained_bytes()).sum::<usize>()).sum::<usize>()).sum::<usize>()
}
fn database_bytes(database: &Database) -> usize { database.stores.iter().map(|(name, store)| store_bytes(name, store)).sum() }

/// Persistence boundary. Implementations must atomically compare the expected
/// revision and replace the entire snapshot, or leave the old snapshot intact.
/// A missing database has revision zero. Names are partitioned by the host's
/// storage key; this core must never infer a key from a display URL.
pub trait Backend: Send {
    fn load(&self, storage_key: &str, name: &str) -> Result<Option<Database>, Error>;
    fn replace(&mut self, storage_key: &str, name: &str, expected_revision: u64, value: Option<Database>) -> Result<(), Error>;
    fn list(&self, storage_key: &str) -> Result<Vec<(String, u64)>, Error>;
}

/// Explicit bounded, process-local host backend. Browser persistence requires
/// a durable implementation of `Backend`; this type does not imply durability.
pub struct MemoryBackend { databases: BTreeMap<(String, String), Database>, max_databases: usize, max_bytes: usize }
impl MemoryBackend {
    pub fn new(max_databases: usize, max_bytes: usize) -> Self { Self { databases: BTreeMap::new(), max_databases, max_bytes } }
}
impl Backend for MemoryBackend {
    fn load(&self, storage_key: &str, name: &str) -> Result<Option<Database>, Error> { Ok(self.databases.get(&(storage_key.into(), name.into())).cloned()) }
    fn replace(&mut self, storage_key: &str, name: &str, expected_revision: u64, value: Option<Database>) -> Result<(), Error> {
        let key = (storage_key.to_owned(), name.to_owned());
        if self.databases.get(&key).map_or(0, |db| db.revision) != expected_revision { return Err(Error::Conflict); }
        if let Some(mut database) = value {
            if !self.databases.contains_key(&key) && self.databases.len() >= self.max_databases { return Err(Error::QuotaExceeded); }
            // Include key/name allocations as well as clone bytes in admission.
            let used = self.databases.iter().filter(|(existing, _)| *existing != &key).map(|((partition, name), db)| partition.len() + name.len() + database_bytes(db)).sum::<usize>();
            if used.saturating_add(storage_key.len()).saturating_add(name.len()).saturating_add(database_bytes(&database)) > self.max_bytes { return Err(Error::QuotaExceeded); }
            database.revision = expected_revision.checked_add(1).ok_or(Error::QuotaExceeded)?;
            self.databases.insert(key, database);
        } else { self.databases.remove(&key); }
        Ok(())
    }
    fn list(&self, storage_key: &str) -> Result<Vec<(String, u64)>, Error> { Ok(self.databases.iter().filter(|((key, _), _)| key == storage_key).map(|((_, name), db)| (name.clone(), db.version)).collect()) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { ReadOnly, ReadWrite, VersionChange }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State { Active, Committed, Aborted }
pub struct Transaction { pub database: Database, pub scope: Vec<String>, pub mode: Mode, pub state: State, expected_revision: u64, retained_bytes: usize }
impl Transaction {
    pub fn begin(database: Database, scope: Vec<String>, mode: Mode) -> Result<Self, Error> {
        if mode != Mode::VersionChange && (scope.is_empty() || scope.iter().any(|name| !database.stores.contains_key(name))) { return Err(Error::NotFound); }
        let expected_revision = database.revision;
        let retained_bytes = database_bytes(&database);
        Ok(Self { database, scope, mode, state: State::Active, expected_revision, retained_bytes })
    }
    fn writable(&self) -> Result<(), Error> {
        if self.state != State::Active { return Err(Error::TransactionInactive); }
        if self.mode == Mode::ReadOnly { return Err(Error::ReadOnly); }
        Ok(())
    }
    pub fn create_store(&mut self, name: &str, key_path: Option<KeyPath>, auto_increment: bool) -> Result<(), Error> {
        self.writable()?;
        if self.mode != Mode::VersionChange { return Err(Error::InvalidState); }
        if self.database.stores.contains_key(name) { return Err(Error::Constraint); }
        if let Some(path) = &key_path { path.validate()?; if auto_increment && (matches!(path, KeyPath::Sequence(_)) || matches!(path, KeyPath::String(path) if path.is_empty())) { return Err(Error::InvalidAccess); } }
        let store = ObjectStore { key_path, auto_increment, generator: 1, records: Arc::new(BTreeMap::new()), indexes: BTreeMap::new() };
        if self.database.stores.len() >= 256 || self.retained_bytes.saturating_add(store_bytes(name, &store)) > 64 * 1024 * 1024 { return Err(Error::QuotaExceeded); }
        self.retained_bytes += store_bytes(name, &store);
        self.database.stores.insert(name.into(), store);
        Ok(())
    }
    pub fn delete_store(&mut self, name: &str) -> Result<(), Error> {
        self.writable()?;
        if self.mode != Mode::VersionChange { return Err(Error::InvalidState); }
        let store = self.database.stores.remove(name).ok_or(Error::NotFound)?;
        let removed = store_bytes(name, &store);
        self.retained_bytes = self.retained_bytes.saturating_sub(removed);
        Ok(())
    }
    pub fn store(&self, name: &str) -> Result<&ObjectStore, Error> {
        if self.state != State::Active { return Err(Error::TransactionInactive); }
        if self.mode != Mode::VersionChange && !self.scope.iter().any(|candidate| candidate == name) { return Err(Error::NotFound); }
        self.database.stores.get(name).ok_or(Error::NotFound)
    }
    pub fn put(&mut self, name: &str, value: Vec<u8>, key: Option<Key>, overwrite: bool) -> Result<Key, Error> {
        self.put_indexed(name, value, key, overwrite, BTreeMap::new())
    }
    fn replace_store(&mut self, name: &str, store: ObjectStore) -> Result<(), Error> {
        let old = self.database.stores.get(name).ok_or(Error::NotFound)?;
        let bytes = self.retained_bytes.saturating_sub(store_bytes(name, old)).saturating_add(store_bytes(name, &store));
        if bytes > 64 * 1024 * 1024 { return Err(Error::QuotaExceeded); }
        self.database.stores.insert(name.into(), store); self.retained_bytes = bytes; Ok(())
    }
    pub fn create_index(&mut self, store: &str, name: &str, key_path: KeyPath, unique: bool, multi_entry: bool) -> Result<(), Error> {
        self.writable()?; self.store(store)?;
        if self.mode != Mode::VersionChange { return Err(Error::InvalidState); }
        key_path.validate()?;
        if multi_entry && matches!(key_path, KeyPath::Sequence(_)) { return Err(Error::InvalidAccess); }
        let mut candidate = self.database.stores[store].clone();
        if candidate.indexes.contains_key(name) { return Err(Error::Constraint); }
        if candidate.indexes.len() >= 128 { return Err(Error::QuotaExceeded); }
        candidate.indexes.insert(name.into(), Index { key_path, unique, multi_entry, records: Arc::new(BTreeMap::new()) });
        self.replace_store(store, candidate)
    }
    pub fn delete_index(&mut self, store: &str, name: &str) -> Result<(), Error> {
        self.writable()?; self.store(store)?;
        if self.mode != Mode::VersionChange { return Err(Error::InvalidState); }
        let mut candidate = self.database.stores[store].clone();
        candidate.indexes.remove(name).ok_or(Error::NotFound)?;
        self.replace_store(store, candidate)
    }
    pub fn populate_index(&mut self, store: &str, name: &str, entries: Vec<(Key, Vec<Key>)>) -> Result<(), Error> {
        self.writable()?; self.store(store)?;
        let input_bytes = entries.iter().fold(0usize, |total, (primary, keys)| keys.iter().fold(total, |total, key| total.saturating_add(primary.retained_bytes()).saturating_add(key.retained_bytes()).saturating_add(96)));
        if input_bytes > 64 * 1024 * 1024 { return Err(Error::QuotaExceeded); }
        let mut candidate = self.database.stores[store].clone();
        let index = candidate.indexes.get_mut(name).ok_or(Error::NotFound)?;
        let mut records: BTreeMap<Key, BTreeSet<Key>> = BTreeMap::new();
        for (primary, keys) in entries { for key in keys {
            let record = records.entry(key).or_default();
            if index.unique && !record.is_empty() && !record.contains(&primary) { return Err(Error::Constraint); }
            record.insert(primary.clone());
        } }
        index.records = Arc::new(records);
        self.replace_store(store, candidate)
    }
    pub fn index_get(&self, store: &str, name: &str, range: &KeyRange) -> Result<Option<(Key, Arc<[u8]>)>, Error> {
        range.validate()?;
        let store = self.store(store)?; let index = store.indexes.get(name).ok_or(Error::InvalidState)?;
        let primary = index.records.iter().find(|(key, _)| range.contains(key)).and_then(|(_, primary)| primary.first());
        Ok(primary.and_then(|primary| store.records.get(primary).map(|bytes| (primary.clone(), bytes.clone()))))
    }
    pub fn generated_key(&self, name: &str) -> Result<Key, Error> {
        let store = self.store(name)?;
        if !store.auto_increment { return Err(Error::Data); }
        if store.generator > 9_007_199_254_740_992 { return Err(Error::Constraint); }
        Ok(Key::Number(store.generator as f64))
    }
    pub fn put_indexed(&mut self, name: &str, value: Vec<u8>, key: Option<Key>, overwrite: bool, index_keys: BTreeMap<String, Vec<Key>>) -> Result<Key, Error> {
        self.writable()?; self.store(name)?;
        let mut store = self.database.stores[name].clone();
        let generated = key.is_none();
        let key = match key { Some(key) => key, None if store.auto_increment && store.generator <= 9_007_199_254_740_992 => Key::Number(store.generator as f64), None if store.auto_increment => return Err(Error::Constraint), None => return Err(Error::Data) };
        if !overwrite && store.records.contains_key(&key) { return Err(Error::Constraint); }
        let input_bytes = index_keys.values().flatten().fold(value.len().saturating_add(key.retained_bytes()).saturating_add(64), |total, index_key| total.saturating_add(index_key.retained_bytes()).saturating_add(key.retained_bytes()).saturating_add(96));
        if input_bytes > 64 * 1024 * 1024 { return Err(Error::QuotaExceeded); }
        // Validate every index against the same candidate before publishing
        // any primary record, index record, or generator update.
        for (name, index) in &mut store.indexes {
            let records = Arc::make_mut(&mut index.records);
            records.retain(|_, primary| { primary.remove(&key); !primary.is_empty() });
            for index_key in index_keys.get(name).into_iter().flatten() {
                let primary = records.entry(index_key.clone()).or_default();
                if index.unique && primary.iter().any(|primary| primary != &key) { return Err(Error::Constraint); }
                primary.insert(key.clone());
            }
        }
        if store.auto_increment {
            if generated { store.generator += 1; }
            else if let Key::Number(number) = &key { if *number >= store.generator as f64 { store.generator = number.floor().min(9_007_199_254_740_992.0) as u64 + 1; } }
        }
        Arc::make_mut(&mut store.records).insert(key.clone(), Arc::from(value));
        self.replace_store(name, store)?; Ok(key)
    }
    pub fn get(&self, name: &str, range: &KeyRange) -> Result<Option<(Key, Arc<[u8]>)>, Error> {
        range.validate()?;
        Ok(self.store(name)?.records.iter().find(|(key, _)| range.contains(key)).map(|(key, value)| (key.clone(), value.clone())))
    }
    pub fn delete(&mut self, name: &str, range: &KeyRange) -> Result<(), Error> {
        self.writable()?; range.validate()?; self.store(name)?;
        let mut candidate = self.database.stores[name].clone();
        Arc::make_mut(&mut candidate.records).retain(|key, _| !range.contains(key));
        for index in candidate.indexes.values_mut() { Arc::make_mut(&mut index.records).retain(|_, primary| { primary.retain(|key| !range.contains(key)); !primary.is_empty() }); }
        self.replace_store(name, candidate)
    }
    pub fn abort(&mut self) -> Result<(), Error> {
        if self.state != State::Active { return Err(Error::InvalidState); }
        self.state = State::Aborted; Ok(())
    }
    pub fn commit(&mut self, backend: &mut dyn Backend, storage_key: &str, name: &str) -> Result<(), Error> {
        if self.state != State::Active { return Err(Error::InvalidState); }
        if self.mode != Mode::ReadOnly { backend.replace(storage_key, name, self.expected_revision, Some(self.database.clone()))?; }
        self.state = State::Committed; Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn exact(key: Key) -> KeyRange { KeyRange { lower: Some(key.clone()), upper: Some(key), lower_open: false, upper_open: false } }
    #[test]
    fn atomic_upgrade_write_abort_and_origin_isolation() {
        let mut backend = MemoryBackend::new(8, 1024);
        let mut upgrade = Transaction::begin(Database { version: 1, ..Database::default() }, vec![], Mode::VersionChange).unwrap();
        upgrade.create_store("items", None, true).unwrap();
        let key = upgrade.put("items", vec![1, 2, 3], None, false).unwrap();
        assert_eq!(key, Key::Number(1.0));
        assert!(backend.load("origin-a", "db").unwrap().is_none());
        upgrade.commit(&mut backend, "origin-a", "db").unwrap();
        let db = backend.load("origin-a", "db").unwrap().unwrap();
        let mut aborted = Transaction::begin(db.clone(), vec!["items".into()], Mode::ReadWrite).unwrap();
        aborted.delete("items", &exact(key.clone())).unwrap(); aborted.abort().unwrap();
        assert_eq!(aborted.commit(&mut backend, "origin-a", "db"), Err(Error::InvalidState));
        let reader = Transaction::begin(db, vec!["items".into()], Mode::ReadOnly).unwrap();
        assert_eq!(&*reader.get("items", &exact(key)).unwrap().unwrap().1, &[1, 2, 3]);
        assert!(backend.load("origin-b", "db").unwrap().is_none());
    }
    #[test]
    fn conflicts_and_quota_do_not_partially_commit() {
        let mut backend = MemoryBackend::new(1, 512);
        let db = Database { version: 1, ..Database::default() };
        let mut first = Transaction::begin(db.clone(), vec![], Mode::VersionChange).unwrap();
        let mut stale = Transaction::begin(db, vec![], Mode::VersionChange).unwrap();
        first.create_store("s", None, false).unwrap(); first.commit(&mut backend, "o", "d").unwrap();
        assert_eq!(stale.commit(&mut backend, "o", "d"), Err(Error::Conflict));
        let original = backend.load("o", "d").unwrap().unwrap();
        let mut writer = Transaction::begin(original, vec!["s".into()], Mode::ReadWrite).unwrap();
        writer.put("s", vec![0; 1024], Some(Key::Number(3.0)), true).unwrap();
        assert_eq!(writer.commit(&mut backend, "o", "d"), Err(Error::QuotaExceeded));
        assert!(backend.load("o", "d").unwrap().unwrap().stores["s"].records.is_empty());
    }
    #[test]
    fn snapshot_shares_records_and_metadata_admission_releases_deleted_store() {
        let mut transaction = Transaction::begin(Database { version: 1, ..Database::default() }, vec![], Mode::VersionChange).unwrap();
        transaction.create_store("s", None, false).unwrap();
        let key = Key::Binary(vec![0; 1024]);
        transaction.put("s", vec![1; 128], Some(key.clone()), true).unwrap();
        assert_eq!(transaction.retained_bytes, 129 + 1024 + 128 + 64);
        let snapshot = transaction.database.clone();
        assert!(Arc::ptr_eq(&snapshot.stores["s"].records, &transaction.database.stores["s"].records));
        transaction.delete("s", &exact(key.clone())).unwrap();
        assert_eq!(transaction.retained_bytes, 129);
        assert_eq!(snapshot.stores["s"].records.len(), 1);
        transaction.put("s", vec![1; 128], Some(key), true).unwrap();
        transaction.delete_store("s").unwrap();
        assert_eq!(transaction.retained_bytes, 0);
        transaction.create_store("large", None, true).unwrap();
        assert_eq!(transaction.put("large", vec![], Some(Key::Binary(vec![0; 64 * 1024 * 1024])), true), Err(Error::QuotaExceeded));
        assert_eq!(transaction.database.stores["large"].generator, 1);
        assert!(transaction.database.stores["large"].records.is_empty());
    }
    #[test]
    fn index_uniqueness_is_atomic_and_deletion_removes_secondary_records() {
        let mut transaction = Transaction::begin(Database { version: 1, ..Database::default() }, vec![], Mode::VersionChange).unwrap();
        transaction.create_store("s", None, true).unwrap();
        transaction.create_index("s", "email", KeyPath::String("email".into()), true, false).unwrap();
        let index_key = Key::String("a".encode_utf16().collect());
        let keys = BTreeMap::from([("email".into(), vec![index_key.clone()])]);
        let primary = transaction.put_indexed("s", vec![42], None, false, keys.clone()).unwrap();
        let snapshot = transaction.database.clone();
        let oversized = BTreeMap::from([("email".into(), vec![Key::Binary(vec![0; 64 * 1024 * 1024])])]);
        assert_eq!(transaction.put_indexed("s", vec![9], None, false, oversized), Err(Error::QuotaExceeded));
        assert!(Arc::ptr_eq(&snapshot.stores["s"].indexes["email"].records, &transaction.database.stores["s"].indexes["email"].records));
        assert_eq!(transaction.put_indexed("s", vec![9], None, false, keys), Err(Error::Constraint));
        assert_eq!(transaction.database.stores["s"].generator, 2);
        assert_eq!(transaction.database.stores["s"].records.len(), 1);
        assert_eq!(transaction.index_get("s", "email", &exact(index_key.clone())).unwrap().unwrap().0, primary);
        transaction.delete("s", &exact(primary)).unwrap();
        assert!(transaction.index_get("s", "email", &exact(index_key.clone())).unwrap().is_none());
        assert_eq!(snapshot.stores["s"].indexes["email"].records.len(), 1);
        assert_eq!(transaction.create_index("s", "bad", KeyPath::Sequence(vec!["email".into()]), false, true), Err(Error::InvalidAccess));
        assert_eq!(KeyPath::String("a..b".into()).validate(), Err(Error::Syntax));
        assert_eq!(KeyPath::Sequence(vec![]).validate(), Err(Error::Syntax));
        assert!(KeyPath::String("α.$valid\u{200D}".into()).validate().is_ok());
    }
    #[test]
    fn key_order_and_open_bounds_are_indexed_db_order() {
        let mut keys = vec![Key::Array(vec![]), Key::Binary(vec![]), Key::String(vec![]), Key::Date(0.0), Key::Number(0.0)];
        keys.sort(); assert!(matches!(keys[0], Key::Number(_)));
        assert!(matches!(keys[1], Key::Date(_))); assert!(matches!(keys[4], Key::Array(_)));
        assert!(Key::number(f64::NAN).is_err()); assert_eq!(Key::number(-0.0), Key::number(0.0));
        let range = KeyRange { lower: Some(Key::Number(1.0)), upper: Some(Key::Number(3.0)), lower_open: true, upper_open: false };
        assert!(!range.contains(&Key::Number(1.0))); assert!(range.contains(&Key::Number(3.0)));
    }
}
