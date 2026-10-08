//! Typed IndexedDB bindings. The embedder supplies storage partitioning and
//! task admission; records and atomic snapshots live in lumen-common.
use crate::events::{self, Event, EventTarget, TargetData};
use crate::realm_services::RealmServices;
use lumen::embed::{Ctx, JsHost, OpError, OpResult, Value};
use lumen_bind::This;
use lumen_common::indexed_db as core;
use std::{cell::{Cell, RefCell}, rc::{Rc, Weak}, collections::VecDeque};
use std::sync::{Arc, Mutex};

pub type Task = Box<dyn FnOnce(&mut Ctx) -> OpResult<()>>;
pub type TaskAdmission = Rc<dyn Fn(&mut Ctx, Task) -> OpResult<()>>;
pub type SharedBackend = Arc<Mutex<dyn core::Backend>>;
pub struct Environment {
    pub storage_key: Rc<dyn Fn(&mut Ctx) -> OpResult<String>>,
    pub backend: SharedBackend,
    pub queue: TaskAdmission,
}
struct Service { environment: Environment, connections: RefCell<Vec<Weak<Connection>>>, pending: RefCell<VecDeque<PendingOpen>> }
#[derive(Default)]
struct ActiveTransactions(Vec<Weak<TransactionState>>);
struct PendingOpen { storage_key: String, name: String, version: Option<u64>, target: Value, state: Rc<RequestState>, deleting: bool }
pub fn install(ctx: &mut Ctx, environment: Environment) -> OpResult<()> {
    RealmServices::replace_current(ctx, Service { environment, connections: RefCell::new(Vec::new()), pending: RefCell::new(VecDeque::new()) });
    crate::globals::<bindings::Module>(ctx).map_err(OpError::thrown)?;
    let global = ctx.global_object();
    let service = service(ctx)?;
    let factory = traced(ctx, bindings::IDBFactory { service })?;
    ctx.member_set(&global, "indexedDB", factory).map_err(OpError::thrown)
}
fn service(ctx: &mut Ctx) -> OpResult<Rc<Service>> {
    RealmServices::current(ctx).ok_or_else(|| OpError::new("InvalidStateError", "IndexedDB host environment unavailable"))
}
fn traced<T: lumen_bind::Methods<JsHost> + lumen::embed::NativeIdentityOwner>(ctx: &mut Ctx, native: T) -> OpResult<Value> {
    let value = ctx.new_instance(native);
    ctx.set_native_identity_owner::<T>(&value)?;
    Ok(value)
}
fn error(ctx: &mut Ctx, error: core::Error) -> OpError { OpError::thrown(events::dom_exception(ctx, error.name(), error.name())) }
fn event(ctx: &mut Ctx, receiver: &Value, kind: &str) -> OpResult<bool> {
    let event = traced(ctx, Event::from_init(kind, events::EventInit { bubbles: kind == "abort", ..events::EventInit::default() }))?;
    EventTarget::dispatch_trusted(ctx, receiver, &event)
}
fn version_event(ctx: &mut Ctx, receiver: &Value, kind: &str, old: u64, new: Option<u64>) -> OpResult<()> {
    let value = traced(ctx, bindings::IDBVersionChangeEvent { base: Event::from_init(kind, events::EventInit::default()), old, new })?;
    EventTarget::dispatch_trusted(ctx, receiver, &value)?; Ok(())
}
fn queue(ctx: &mut Ctx, service: &Rc<Service>, task: impl FnOnce(&mut Ctx) -> OpResult<()> + 'static) -> OpResult<()> {
    (service.environment.queue)(ctx, Box::new(task))
}
fn key(ctx: &mut Ctx, value: &Value, depth: usize) -> OpResult<core::Key> {
    if depth > 128 { return Err(error(ctx, core::Error::Data)); }
    match value {
        Value::Num(number) => core::Key::number(*number).map_err(|e| error(ctx, e)),
        Value::Str(_) => Ok(core::Key::String(ctx.coerce_string(value).map_err(OpError::thrown)?.encode_utf16().collect())),
        Value::Obj(_) if ctx.is_array_value(value).map_err(OpError::thrown)? => {
            let length = ctx.member_get(value, "length").map_err(OpError::thrown)?;
            let Value::Num(length) = length else { return Err(error(ctx, core::Error::Data)); };
            if length > 4096.0 { return Err(error(ctx, core::Error::QuotaExceeded)); }
            let mut keys = Vec::new();
            for index in 0..length as usize {
                if !ctx.has_own_property_value(value, &Value::str(index.to_string())).map_err(OpError::thrown)? { return Err(error(ctx, core::Error::Data)); }
                let item = ctx.member_get(value, &index.to_string()).map_err(OpError::thrown)?;
                keys.push(key(ctx, &item, depth + 1)?);
            }
            Ok(core::Key::Array(keys))
        }
        Value::Obj(_) if ctx.date_value(value).is_some() => {
            let time = ctx.date_value(value).unwrap();
            if time.is_nan() { Err(error(ctx, core::Error::Data)) } else { Ok(core::Key::Date(time)) }
        }
        Value::Obj(_) if ctx.buffer_source_bytes(value).is_some() => Ok(core::Key::Binary(ctx.buffer_source_bytes(value).unwrap())),
        _ => Err(error(ctx, core::Error::Data)),
    }
}
fn key_value(ctx: &mut Ctx, key: &core::Key) -> Value {
    match key {
        core::Key::Number(n) => Value::Num(*n), core::Key::Date(n) => ctx.new_date_value(*n),
        core::Key::String(text) => Value::str(String::from_utf16_lossy(text)),
        core::Key::Array(keys) => { let values = keys.iter().map(|key| key_value(ctx, key)).collect(); ctx.make_array(values) }
        core::Key::Binary(bytes) => ctx.make_array_buffer_from(bytes.clone()),
    }
}
fn range(ctx: &mut Ctx, value: &Value) -> OpResult<core::KeyRange> {
    if let Ok(range) = ctx.with_instance::<bindings::IDBKeyRange, _>(value, |range| range.range.clone()) { return Ok(range); }
    let key = key(ctx, value, 0)?;
    Ok(core::KeyRange { lower: Some(key.clone()), upper: Some(key), lower_open: false, upper_open: false })
}
fn key_path(ctx: &mut Ctx, value: &Value) -> OpResult<core::KeyPath> {
    let path = if ctx.is_array_value(value).map_err(OpError::thrown)? {
        let Value::Num(length) = ctx.member_get(value, "length").map_err(OpError::thrown)? else { return Err(OpError::type_error("Invalid key path")); };
        if length > 4096.0 { return Err(error(ctx, core::Error::QuotaExceeded)); }
        let mut paths = Vec::new();
        for index in 0..length as usize { let value = ctx.member_get(value, &index.to_string()).map_err(OpError::thrown)?; paths.push(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()); }
        core::KeyPath::Sequence(paths)
    } else { core::KeyPath::String(ctx.coerce_string(value).map_err(OpError::thrown)?.to_string()) };
    path.validate().map_err(|reason| error(ctx, reason))?; Ok(path)
}
fn path_value(ctx: &mut Ctx, value: &Value, path: &str) -> OpResult<Option<Value>> {
    if path.is_empty() { return Ok(Some(value.clone())); }
    let mut value = value.clone();
    for segment in path.split('.') {
        if !matches!(value, Value::Obj(_) | Value::Str(_)) { return Ok(None); }
        if matches!(value, Value::Obj(_)) && !ctx.has_own_property_value(&value, &Value::str(segment)).map_err(OpError::thrown)? { return Ok(None); }
        value = ctx.member_get(&value, segment).map_err(OpError::thrown)?;
        if matches!(value, Value::Undefined) { return Ok(None); }
    }
    Ok(Some(value))
}
fn path_key(ctx: &mut Ctx, value: &Value, path: &core::KeyPath) -> OpResult<Option<core::Key>> {
    match path {
        core::KeyPath::String(path) => path_value(ctx, value, path)?.map(|value| key(ctx, &value, 0)).transpose(),
        core::KeyPath::Sequence(paths) => {
            let mut keys = Vec::new();
            for path in paths { let Some(value) = path_value(ctx, value, path)? else { return Ok(None); }; keys.push(key(ctx, &value, 0)?); }
            Ok(Some(core::Key::Array(keys)))
        }
    }
}
fn path_to_value(ctx: &mut Ctx, path: &core::KeyPath) -> Value { match path { core::KeyPath::String(path) => Value::str(path), core::KeyPath::Sequence(paths) => ctx.make_array(paths.iter().map(Value::str).collect()) } }
fn inject_key(ctx: &mut Ctx, value: &Value, path: &str, key: Value) -> OpResult<()> {
    if path.is_empty() || !matches!(value, Value::Obj(_)) { return Err(error(ctx, core::Error::Data)); }
    let parts: Vec<_> = path.split('.').collect(); let mut parent = value.clone();
    for part in &parts[..parts.len() - 1] {
        if !matches!(parent, Value::Obj(_)) { return Err(error(ctx, core::Error::Data)); }
        let own = ctx.has_own_property_value(&parent, &Value::str(*part)).map_err(OpError::thrown)?;
        let next = if own { ctx.member_get(&parent, part).map_err(OpError::thrown)? } else { let object = Value::Obj(ctx.new_object()); ctx.member_set(&parent, part, object.clone()).map_err(OpError::thrown)?; object };
        if !matches!(next, Value::Obj(_)) { return Err(error(ctx, core::Error::Data)); } parent = next;
    }
    ctx.member_set(&parent, parts[parts.len() - 1], key).map_err(OpError::thrown)
}
fn index_keys(ctx: &mut Ctx, value: &Value, index: &core::Index) -> OpResult<Vec<core::Key>> {
    if index.multi_entry {
        let core::KeyPath::String(path) = &index.key_path else { return Err(error(ctx, core::Error::InvalidAccess)); };
        let Some(extracted) = path_value(ctx, value, path)? else { return Ok(vec![]); };
        if ctx.is_array_value(&extracted).map_err(OpError::thrown)? {
            let Value::Num(length) = ctx.member_get(&extracted, "length").map_err(OpError::thrown)? else { return Ok(vec![]); };
            if length > 4096.0 { return Err(error(ctx, core::Error::QuotaExceeded)); }
            let mut keys = std::collections::BTreeSet::new();
            for index in 0..length as usize {
                if !ctx.has_own_property_value(&extracted, &Value::str(index.to_string())).map_err(OpError::thrown)? { continue; }
                let item = ctx.member_get(&extracted, &index.to_string()).map_err(OpError::thrown)?;
                let candidate = key(ctx, &item, 0);
                if let Some(key) = optional_index_key(ctx, candidate)? { keys.insert(key); }
            }
            return Ok(keys.into_iter().collect());
        }
    }
    // Invalid index keys omit an index entry; they do not reject the record.
    let result = path_key(ctx, value, &index.key_path);
    match result { Ok(key) => Ok(key.into_iter().collect()), Err(error) => Ok(optional_index_key(ctx, Err(error))?.into_iter().collect()) }
}
fn optional_index_key(ctx: &mut Ctx, result: OpResult<core::Key>) -> OpResult<Option<core::Key>> {
    match result { Ok(key) => Ok(Some(key)), Err(failure) => {
        let value = failure.to_value(ctx); let name = ctx.member_get(&value, "name").map_err(OpError::thrown)?;
        if ctx.coerce_string(&name).map_err(OpError::thrown)?.as_ref() == "DataError" { Ok(None) } else { Err(OpError::thrown(value)) }
    } }
}

struct RequestState { done: Cell<bool>, result: RefCell<Value>, error: RefCell<Value>, source: Value, transaction: RefCell<Value>, parent: RefCell<Weak<TransactionState>> }
impl RequestState {
    fn new(source: Value, transaction: Value) -> Rc<Self> { Rc::new(Self { done: Cell::new(false), result: RefCell::new(Value::Undefined), error: RefCell::new(Value::Null), source, transaction: RefCell::new(transaction), parent: RefCell::new(Weak::new()) }) }
    fn trace(&self, visit: &mut dyn FnMut(&Value)) { visit(&self.result.borrow()); visit(&self.error.borrow()); visit(&self.source); visit(&self.transaction.borrow()); }
}
struct Connection {
    service: Rc<Service>, storage_key: String, name: String, version: Cell<u64>, closed: Cell<bool>,
    closing: Cell<bool>, transactions: Cell<usize>,
    previous_version: u64,
    upgrade: RefCell<Option<Rc<TransactionState>>>,
    wrapper: RefCell<Option<lumen::embed::WeakValue>>,
    target: Rc<TargetData>,
}
struct TransactionState {
    connection: Rc<Connection>, core: RefCell<core::Transaction>, pending: Cell<usize>,
    database: Value,
    active: Cell<bool>, finished: Cell<bool>, wrapper: RefCell<Value>,
    error: RefCell<Value>, open: RefCell<Option<(Value, Rc<RequestState>)>>,
    dispatch_failed: Cell<bool>, target: Rc<TargetData>,
}
struct RequestTarget(Weak<RequestState>);
impl events::TargetHooks for RequestTarget {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn listener_exception(&self) {
        if let Some(transaction) = self.0.upgrade().and_then(|request| request.parent.borrow().upgrade()) { transaction.dispatch_failed.set(true); }
    }
    fn event_path(&self, _: &mut Ctx, data: &Rc<TargetData>, receiver: &Value, event: &Event, initial: &Value) -> OpResult<Option<events::EventPath>> {
        let Some(request) = self.0.upgrade() else { return Ok(None); };
        // Open requests have no event parent, including during an upgrade.
        if matches!(request.source, Value::Null) { return Ok(None); }
        let Some(transaction) = request.parent.borrow().upgrade() else { return Ok(None); };
        let mut entries = vec![events::PathEntry { value: receiver.clone(), target: data.clone(), adjusted: initial.clone(), closed: vec![], related: event.related_original() }];
        let parent = transaction.wrapper.borrow().clone();
        if matches!(parent, Value::Obj(_)) { entries.push(events::PathEntry { value: parent, target: transaction.target.clone(), adjusted: initial.clone(), closed: vec![], related: event.related_original() }); }
        if let Some(parent) = transaction.connection.wrapper.borrow().as_ref().and_then(lumen::embed::WeakValue::upgrade) {
            entries.push(events::PathEntry { value: parent, target: transaction.connection.target.clone(), adjusted: initial.clone(), closed: vec![], related: event.related_original() });
        }
        Ok(Some(events::EventPath { entries, clear_target: false }))
    }
}
fn request_target(state: &Rc<RequestState>) -> EventTarget { EventTarget::from_data(TargetData::new(Some(Rc::new(RequestTarget(Rc::downgrade(state)))))) }
struct TransactionTarget(Weak<TransactionState>);
impl events::TargetHooks for TransactionTarget {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn event_path(&self, _: &mut Ctx, data: &Rc<TargetData>, receiver: &Value, event: &Event, initial: &Value) -> OpResult<Option<events::EventPath>> {
        let Some(transaction) = self.0.upgrade() else { return Ok(None); };
        Ok(Some(events::EventPath { entries: vec![
            events::PathEntry { value: receiver.clone(), target: data.clone(), adjusted: initial.clone(), closed: vec![], related: event.related_original() },
            events::PathEntry { value: transaction.database.clone(), target: transaction.connection.target.clone(), adjusted: initial.clone(), closed: vec![], related: event.related_original() },
        ], clear_target: false }))
    }
}
fn new_transaction(ctx: &mut Ctx, connection: &Rc<Connection>, snapshot: core::Database, scope: Vec<String>, mode: core::Mode) -> OpResult<(Value, Rc<TransactionState>)> {
    let transaction = core::Transaction::begin(snapshot, scope, mode).map_err(|e| error(ctx, e))?;
    let database = connection.wrapper.borrow().as_ref().and_then(lumen::embed::WeakValue::upgrade).ok_or_else(|| error(ctx, core::Error::InvalidState))?;
    let state = Rc::new_cyclic(|weak| TransactionState { connection: connection.clone(), database, core: RefCell::new(transaction), pending: Cell::new(0), active: Cell::new(true), finished: Cell::new(false), wrapper: RefCell::new(Value::Undefined), error: RefCell::new(Value::Null), open: RefCell::new(None), dispatch_failed: Cell::new(false), target: TargetData::new(Some(Rc::new(TransactionTarget(weak.clone())))) });
    let value = traced(ctx, bindings::IDBTransaction { base: EventTarget::from_data(state.target.clone()), state: state.clone() })?;
    *state.wrapper.borrow_mut() = value.clone();
    track_active(ctx, &state);
    connection.transactions.set(connection.transactions.get() + 1);
    if let Err(failure) = finalize_later(ctx, &state) {
        connection.transactions.set(connection.transactions.get() - 1);
        *state.wrapper.borrow_mut() = Value::Undefined;
        return Err(failure);
    }
    Ok((value, state))
}
fn finalize_later(ctx: &mut Ctx, state: &Rc<TransactionState>) -> OpResult<()> {
    let state = state.clone(); let service = state.connection.service.clone();
    // A queued task owns an external lease, independent of native GC edges.
    let transaction = state.wrapper.borrow().clone();
    queue(ctx, &service, move |ctx| {
        state.active.set(false);
        if state.finished.get() || state.pending.get() > 0 { return Ok(()); }
        let connection = &state.connection;
        let commit = state.core.borrow_mut().commit(&mut *connection.service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?, &connection.storage_key, &connection.name);
        if let Err(reason) = commit {
            let failure = events::dom_exception(ctx, reason.name(), reason.name());
            return abort_transaction(ctx, &state, failure);
        }
        state.finished.set(true);
        connection.upgrade.borrow_mut().take();
        event(ctx, &transaction, "complete")?;
        if let Some((request, request_state)) = state.open.borrow_mut().take() {
            *request_state.transaction.borrow_mut() = Value::Null;
            request_state.done.set(false);
            queue(ctx, &connection.service, move |ctx| {
                request_state.done.set(true);
                event(ctx, &request, "success")?;
                Ok(())
            })?;
        }
        finish_connection(ctx, connection)?;
        Ok(())
    })
}
fn abort_transaction(ctx: &mut Ctx, state: &Rc<TransactionState>, reason: Value) -> OpResult<()> {
    if state.finished.replace(true) { return Ok(()); }
    state.active.set(false);
    let _ = state.core.borrow_mut().abort();
    *state.error.borrow_mut() = reason;
    let transaction = state.wrapper.borrow().clone();
    let state = state.clone(); let service = state.connection.service.clone();
    queue(ctx, &service, move |ctx| {
        state.connection.upgrade.borrow_mut().take();
        if state.open.borrow().is_some() {
            state.connection.closed.set(true);
            state.connection.closing.set(true);
            state.connection.version.set(state.connection.previous_version);
        }
        event(ctx, &transaction, "abort")?;
        if let Some((request, request_state)) = state.open.borrow_mut().take() {
            *request_state.transaction.borrow_mut() = Value::Null;
            *request_state.result.borrow_mut() = Value::Undefined;
            *request_state.error.borrow_mut() = events::dom_exception(ctx, "Upgrade transaction aborted", "AbortError");
            request_state.done.set(false);
            let service = state.connection.service.clone();
            queue(ctx, &service, move |ctx| {
                request_state.done.set(true);
                event(ctx, &request, "error")?;
                Ok(())
            })?;
        }
        finish_connection(ctx, &state.connection)?;
        Ok(())
    })
}
fn ensure_active(ctx: &mut Ctx, state: &TransactionState, write: bool) -> OpResult<()> {
    if state.finished.get() || !state.active.get() { return Err(error(ctx, core::Error::TransactionInactive)); }
    if write && state.core.borrow().mode == core::Mode::ReadOnly { return Err(error(ctx, core::Error::ReadOnly)); }
    Ok(())
}
fn request(ctx: &mut Ctx, source: Value, state: &Rc<TransactionState>, operation: impl FnOnce(&mut Ctx, &mut core::Transaction) -> OpResult<Value> + 'static) -> OpResult<Value> {
    ensure_active(ctx, state, false)?;
    let request_state = RequestState::new(source, state.wrapper.borrow().clone());
    *request_state.parent.borrow_mut() = Rc::downgrade(state);
    let value = traced(ctx, bindings::IDBRequest { base: request_target(&request_state), state: request_state.clone() })?;
    let target = value.clone(); let transaction = state.clone(); let service = state.connection.service.clone();
    state.pending.set(state.pending.get() + 1);
    if let Err(failure) = queue(ctx, &service, move |ctx| {
        transaction.pending.set(transaction.pending.get() - 1);
        let result = if transaction.finished.get() { Err(error(ctx, core::Error::Abort)) }
            else { operation(ctx, &mut transaction.core.borrow_mut()) };
        request_state.done.set(true);
        if !transaction.finished.get() && !transaction.active.replace(true) { track_active(ctx, &transaction); }
        match result {
            Ok(value) => { *request_state.result.borrow_mut() = value; event(ctx, &target, "success")?; }
            Err(failure) => {
                *request_state.error.borrow_mut() = failure.to_value(ctx);
                let event_value = traced(ctx, Event::from_init("error", events::EventInit { cancelable: true, bubbles: true, ..Default::default() }))?;
                if EventTarget::dispatch_trusted(ctx, &target, &event_value)? {
                    let reason = request_state.error.borrow().clone();
                    abort_transaction(ctx, &transaction, reason)?;
                }
            }
        }
        if transaction.dispatch_failed.replace(false) {
            let reason = events::dom_exception(ctx, "Request event handler threw", "AbortError");
            abort_transaction(ctx, &transaction, reason)?;
        }
        finalize_later(ctx, &transaction)
    }) {
        state.pending.set(state.pending.get() - 1); return Err(failure);
    }
    Ok(value)
}

#[lumen_bind::module(name = "indexedDBBindings")]
pub mod bindings {
    use super::*;
    #[class(name = "IDBFactory", hint(js(webidl, invalid_this)))]
    pub struct IDBFactory { pub(super) service: Rc<Service> }
    #[class(name = "IDBRequest", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct IDBRequest { pub(super) base: EventTarget, pub(super) state: Rc<RequestState> }
    #[class(name = "IDBOpenDBRequest", extends = IDBRequest, hint(js(webidl, invalid_this)))]
    pub struct IDBOpenDBRequest { pub(super) base: IDBRequest }
    #[class(name = "IDBDatabase", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct IDBDatabase { pub(super) base: EventTarget, pub(super) connection: Rc<Connection> }
    #[class(name = "IDBTransaction", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct IDBTransaction { pub(super) base: EventTarget, pub(super) state: Rc<TransactionState> }
    #[class(name = "IDBObjectStore", hint(js(webidl, invalid_this)))]
    pub struct IDBObjectStore { name: String, pub(super) state: Rc<TransactionState> }
    #[class(name = "IDBIndex", hint(js(webidl, invalid_this)))]
    pub struct IDBIndex { name: String, store: String, pub(super) object_store: Value, pub(super) state: Rc<TransactionState> }
    #[class(name = "IDBVersionChangeEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct IDBVersionChangeEvent { pub(super) base: Event, pub(super) old: u64, pub(super) new: Option<u64> }
    #[class(name = "IDBKeyRange", hint(js(webidl, invalid_this)))]
    pub struct IDBKeyRange { pub(super) range: core::KeyRange }
    #[class(name = "DOMStringList", hint(js(webidl, invalid_this)))]
    pub struct DOMStringList { names: Vec<String> }

    #[methods]
    impl DOMStringList {
        #[getter] fn length(&self) -> u32 { self.names.len() as u32 }
        fn contains(&self, name: &str) -> bool { self.names.iter().any(|entry| entry == name) }
        #[method(coerce)]
        fn item(&self, index: u32) -> Value { self.names.get(index as usize).map_or(Value::Null, |name| Value::str(name.clone())) }
        #[proto(getitem)] fn get(&self, index: usize) -> Option<String> { self.names.get(index).cloned() }
        #[proto(len)] fn len(&self) -> usize { self.names.len() }
    }
    #[methods]
    impl IDBFactory {
        #[method(coerce)]
        fn open(&self, ctx: &mut Ctx, name: &str, #[default(Value::Undefined)] version: Value) -> OpResult<Value> {
            let storage_key = (self.service.environment.storage_key)(ctx)?;
            let version = if matches!(version, Value::Undefined) { None } else {
                let number = ctx.coerce_number(&version).map_err(OpError::thrown)?;
                if !number.is_finite() || number < 1.0 || number > u64::MAX as f64 { return Err(OpError::type_error("IndexedDB version must be a positive unsigned long long")); }
                Some(number.floor() as u64)
            };
            let state = RequestState::new(Value::Null, Value::Null);
            let value = traced(ctx, IDBOpenDBRequest { base: IDBRequest { base: request_target(&state), state: state.clone() } })?;
            let target = value.clone(); let service = self.service.clone(); let name = name.to_owned();
            queue(ctx, &self.service, move |ctx| open_database(ctx, &service, &storage_key, &name, version, target, state))?;
            Ok(value)
        }
        #[method(coerce)]
        fn delete_database(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
            let storage_key = (self.service.environment.storage_key)(ctx)?;
            let state = RequestState::new(Value::Null, Value::Null);
            let value = traced(ctx, IDBOpenDBRequest { base: IDBRequest { base: request_target(&state), state: state.clone() } })?;
            let target = value.clone(); let service = self.service.clone(); let name = name.to_owned();
            queue(ctx, &self.service, move |ctx| delete_database(ctx, &service, &storage_key, &name, target, state))?; Ok(value)
        }
        fn cmp(&self, ctx: &mut Ctx, first: Value, second: Value) -> OpResult<i32> {
            let first = key(ctx, &first, 0)?; let second = key(ctx, &second, 0)?;
            Ok(match first.cmp(&second) { std::cmp::Ordering::Less => -1, std::cmp::Ordering::Equal => 0, std::cmp::Ordering::Greater => 1 })
        }
    }
    #[methods]
    impl IDBRequest {
        #[getter] fn result(&self, ctx: &mut Ctx) -> OpResult<Value> {
            if !self.state.done.get() { return Err(error(ctx, core::Error::InvalidState)); }
            Ok(self.state.result.borrow().clone())
        }
        #[getter] fn error(&self, ctx: &mut Ctx) -> OpResult<Value> {
            if !self.state.done.get() { return Err(error(ctx, core::Error::InvalidState)); }
            Ok(self.state.error.borrow().clone())
        }
        #[getter] fn source(&self) -> Value { self.state.source.clone() }
        #[getter] fn transaction(&self) -> Value { self.state.transaction.borrow().clone() }
        #[getter] fn ready_state(&self) -> String { if self.state.done.get() { "done" } else { "pending" }.into() }
        #[getter] fn onsuccess(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "success") }
        #[setter] fn set_onsuccess(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "success", value) }
        #[getter] fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "error") }
        #[setter] fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "error", value) }
    }
    #[methods]
    impl IDBOpenDBRequest {
        #[getter] fn onupgradeneeded(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "upgradeneeded") }
        #[setter] fn set_onupgradeneeded(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "upgradeneeded", value) }
        #[getter] fn onblocked(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "blocked") }
        #[setter] fn set_onblocked(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "blocked", value) }
    }
    #[methods]
    impl IDBVersionChangeEvent {
        #[getter] fn old_version(&self) -> f64 { self.old as f64 }
        #[getter] fn new_version(&self) -> Value { self.new.map_or(Value::Null, |v| Value::Num(v as f64)) }
    }
    #[methods]
    impl IDBDatabase {
        #[getter] fn name(&self) -> String { self.connection.name.clone() }
        #[getter] fn version(&self) -> f64 { self.connection.version.get() as f64 }
        #[getter] fn object_store_names(&self, ctx: &mut Ctx) -> OpResult<DOMStringList> {
            let names = if let Some(upgrade) = self.connection.upgrade.borrow().as_ref() { upgrade.core.borrow().database.stores.keys().cloned().collect() }
                else { self.connection.service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?.load(&self.connection.storage_key, &self.connection.name).map_err(|e| error(ctx, e))?.unwrap_or_default().stores.keys().cloned().collect() };
            Ok(DOMStringList { names })
        }
        #[method(coerce)]
        fn create_object_store(&self, ctx: &mut Ctx, name: &str, #[default(Value::Undefined)] options: Value) -> OpResult<Value> {
            let state = self.connection.upgrade.borrow().clone().ok_or_else(|| error(ctx, core::Error::InvalidState))?;
            ensure_active(ctx, &state, true)?;
            let (key_path, auto_increment) = if matches!(options, Value::Undefined | Value::Null) { (None, false) } else {
                let path = ctx.member_get(&options, "keyPath").map_err(OpError::thrown)?;
                let key_path = match path { Value::Undefined | Value::Null => None, _ => Some(key_path(ctx, &path)?) };
                let auto = ctx.member_get(&options, "autoIncrement").map_err(OpError::thrown)?;
                (key_path, ctx.to_boolean(&auto))
            };
            state.core.borrow_mut().create_store(name, key_path, auto_increment).map_err(|e| error(ctx, e))?;
            traced(ctx, IDBObjectStore { name: name.into(), state })
        }
        #[method(coerce)]
        fn delete_object_store(&self, ctx: &mut Ctx, name: &str) -> OpResult<()> {
            let state = self.connection.upgrade.borrow().clone().ok_or_else(|| error(ctx, core::Error::InvalidState))?;
            ensure_active(ctx, &state, true)?;
            let result = state.core.borrow_mut().delete_store(name).map_err(|e| error(ctx, e)); result
        }
        #[method(coerce)]
        fn transaction(&self, ctx: &mut Ctx, stores: Value, #[default("readonly")] mode: &str) -> OpResult<Value> {
            if self.connection.closed.get() || self.connection.closing.get() || self.connection.upgrade.borrow().is_some() { return Err(error(ctx, core::Error::InvalidState)); }
            let scope = if ctx.is_array_value(&stores).map_err(OpError::thrown)? {
                let Value::Num(length) = ctx.member_get(&stores, "length").map_err(OpError::thrown)? else { return Err(OpError::type_error("Invalid store names")); };
                let mut names = Vec::new(); for index in 0..length as usize { let value = ctx.member_get(&stores, &index.to_string()).map_err(OpError::thrown)?; names.push(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()); } names
            } else { vec![ctx.coerce_string(&stores).map_err(OpError::thrown)?.to_string()] };
            let mode = match mode { "readonly" => core::Mode::ReadOnly, "readwrite" => core::Mode::ReadWrite, _ => return Err(OpError::type_error("Invalid transaction mode")) };
            let database = self.connection.service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?.load(&self.connection.storage_key, &self.connection.name).map_err(|e| error(ctx, e))?.ok_or_else(|| error(ctx, core::Error::InvalidState))?;
            Ok(new_transaction(ctx, &self.connection, database, scope, mode)?.0)
        }
        fn close(&self, ctx: &mut Ctx) -> OpResult<()> {
            self.connection.closing.set(true);
            if self.connection.transactions.get() == 0 {
                self.connection.closed.set(true);
                resume_pending(ctx, &self.connection.service)?;
            }
            Ok(())
        }
        #[getter] fn onversionchange(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "versionchange") }
        #[setter] fn set_onversionchange(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "versionchange", value) }
    }
    #[methods]
    impl IDBTransaction {
        #[getter] fn db(&self) -> Value { self.state.database.clone() }
        #[getter] fn mode(&self) -> String { match self.state.core.borrow().mode { core::Mode::ReadOnly => "readonly", core::Mode::ReadWrite => "readwrite", core::Mode::VersionChange => "versionchange" }.into() }
        #[getter] fn error(&self) -> Value { self.state.error.borrow().clone() }
        #[method(coerce)]
        fn object_store(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
            self.state.core.borrow().store(name).map_err(|e| error(ctx, e))?;
            traced(ctx, IDBObjectStore { name: name.into(), state: self.state.clone() })
        }
        fn abort(&self, ctx: &mut Ctx) -> OpResult<()> {
            if self.state.finished.get() { return Err(error(ctx, core::Error::InvalidState)); }
            abort_transaction(ctx, &self.state, Value::Null)
        }
        #[getter] fn oncomplete(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "complete") }
        #[setter] fn set_oncomplete(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "complete", value) }
        #[getter] fn onabort(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "abort") }
        #[setter] fn set_onabort(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "abort", value) }
        #[getter] fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { events::node_handler_get(ctx, &this.0, "error") }
        #[setter] fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> { html_handler(ctx, &this.0, "error", value) }
    }
    #[methods]
    impl IDBObjectStore {
        #[getter] fn name(&self) -> String { self.name.clone() }
        #[getter] fn transaction(&self) -> Value { self.state.wrapper.borrow().clone() }
        #[getter] fn auto_increment(&self) -> bool { self.state.core.borrow().database.stores.get(&self.name).is_some_and(|store| store.auto_increment) }
        #[getter] fn key_path(&self, ctx: &mut Ctx) -> Value { self.state.core.borrow().database.stores.get(&self.name).and_then(|store| store.key_path.as_ref()).map_or(Value::Null, |path| path_to_value(ctx, path)) }
        #[getter] fn index_names(&self) -> DOMStringList { DOMStringList { names: self.state.core.borrow().database.stores.get(&self.name).map(|store| store.indexes.keys().cloned().collect()).unwrap_or_default() } }
        #[method(coerce)]
        fn create_index(&self, ctx: &mut Ctx, this: This<Value>, name: &str, path: Value, #[default(Value::Undefined)] options: Value) -> OpResult<Value> {
            ensure_active(ctx, &self.state, true)?;
            let path = key_path(ctx, &path)?;
            let (unique, multi_entry) = if matches!(options, Value::Undefined | Value::Null) { (false, false) } else {
                let unique = ctx.member_get(&options, "unique").map_err(OpError::thrown)?;
                let multi = ctx.member_get(&options, "multiEntry").map_err(OpError::thrown)?;
                (ctx.to_boolean(&unique), ctx.to_boolean(&multi))
            };
            self.state.core.borrow_mut().create_index(&self.name, name, path, unique, multi_entry).map_err(|reason| error(ctx, reason))?;
            let index = traced(ctx, IDBIndex { name: name.into(), store: self.name.clone(), object_store: this.0, state: self.state.clone() })?;
            let transaction = self.state.clone(); let name = name.to_owned(); let store = self.name.clone();
            let rollback_name = name.clone();
            transaction.pending.set(transaction.pending.get() + 1);
            let lease = transaction.wrapper.borrow().clone();
            if let Err(failure) = queue(ctx, &self.state.connection.service, move |ctx| {
                let _lease = lease;
                transaction.pending.set(transaction.pending.get() - 1);
                if transaction.finished.get() { return Ok(()); }
                let populated = (|| {
                    let (definition, records) = { let core = transaction.core.borrow(); let store = core.store(&store).map_err(|reason| error(ctx, reason))?; let Some(index) = store.indexes.get(&name) else { return Ok(()); }; (index.clone(), store.records.clone()) };
                    let mut entries = Vec::new(); let mut admitted = 0usize;
                    for (primary, bytes) in records.iter() {
                        let value = crate::structured_clone::deserialize_for_storage(ctx, bytes)?; let keys = index_keys(ctx, &value, &definition)?;
                        admitted = keys.iter().fold(admitted, |total, key| total.saturating_add(primary.retained_bytes()).saturating_add(key.retained_bytes()).saturating_add(96));
                        if admitted > 64 * 1024 * 1024 { return Err(error(ctx, core::Error::QuotaExceeded)); }
                        entries.push((primary.clone(), keys));
                    }
                    transaction.core.borrow_mut().populate_index(&store, &name, entries).map_err(|reason| error(ctx, reason))
                })();
                if let Err(failure) = populated { let reason = failure.to_value(ctx); return abort_transaction(ctx, &transaction, reason); }
                finalize_later(ctx, &transaction)
            }) {
                self.state.pending.set(self.state.pending.get() - 1);
                self.state.core.borrow_mut().delete_index(&self.name, &rollback_name).map_err(|reason| error(ctx, reason))?;
                return Err(failure);
            }
            Ok(index)
        }
        #[method(coerce)]
        fn index(&self, ctx: &mut Ctx, this: This<Value>, name: &str) -> OpResult<Value> {
            if !self.state.core.borrow().store(&self.name).map_err(|reason| error(ctx, reason))?.indexes.contains_key(name) { return Err(error(ctx, core::Error::NotFound)); }
            traced(ctx, IDBIndex { name: name.into(), store: self.name.clone(), object_store: this.0, state: self.state.clone() })
        }
        #[method(coerce)]
        fn delete_index(&self, ctx: &mut Ctx, name: &str) -> OpResult<()> { ensure_active(ctx, &self.state, true)?; self.state.core.borrow_mut().delete_index(&self.name, name).map_err(|reason| error(ctx, reason)) }
        fn put(&self, ctx: &mut Ctx, this: This<Value>, value: Value, #[default(Value::Undefined)] key: Value) -> OpResult<Value> { self.write(ctx, this.0, value, key, true) }
        fn add(&self, ctx: &mut Ctx, this: This<Value>, value: Value, #[default(Value::Undefined)] key: Value) -> OpResult<Value> { self.write(ctx, this.0, value, key, false) }
        fn get(&self, ctx: &mut Ctx, this: This<Value>, query: Value) -> OpResult<Value> {
            ensure_active(ctx, &self.state, false)?;
            let query = range(ctx, &query)?; let name = self.name.clone();
            request(ctx, this.0, &self.state, move |ctx, transaction| {
                let value = transaction.get(&name, &query).map_err(|e| error(ctx, e))?;
                match value { Some((_, bytes)) => crate::structured_clone::deserialize_for_storage(ctx, &bytes), None => Ok(Value::Undefined) }
            })
        }
        fn delete(&self, ctx: &mut Ctx, this: This<Value>, query: Value) -> OpResult<Value> {
            ensure_active(ctx, &self.state, true)?;
            let query = range(ctx, &query)?; let name = self.name.clone();
            request(ctx, this.0, &self.state, move |ctx, transaction| { transaction.delete(&name, &query).map_err(|e| error(ctx, e))?; Ok(Value::Undefined) })
        }
    }
    impl IDBObjectStore {
        fn write(&self, ctx: &mut Ctx, source: Value, value: Value, supplied: Value, overwrite: bool) -> OpResult<Value> {
            ensure_active(ctx, &self.state, true)?;
            let store = self.state.core.borrow().store(&self.name).map_err(|e| error(ctx, e))?.clone();
            if store.key_path.is_some() && !matches!(supplied, Value::Undefined) { return Err(error(ctx, core::Error::Data)); }
            let bytes = crate::structured_clone::serialize_for_storage(ctx, &value, 16 * 1024 * 1024)?;
            let cloned = crate::structured_clone::deserialize_for_storage(ctx, &bytes)?;
            let extracted = if let Some(path) = &store.key_path {
                if !matches!(supplied, Value::Undefined) { return Err(error(ctx, core::Error::Data)); }
                let extracted = path_key(ctx, &cloned, path)?;
                if extracted.is_none() {
                    if !store.auto_increment { return Err(error(ctx, core::Error::Data)); }
                    let core::KeyPath::String(path) = path else { return Err(error(ctx, core::Error::Data)); };
                    inject_key(ctx, &cloned, path, Value::Num(0.0))?;
                }
                extracted
            } else if matches!(supplied, Value::Undefined) { None } else { Some(key(ctx, &supplied, 0)?) };
            if extracted.is_none() && !store.auto_increment { return Err(error(ctx, core::Error::Data)); }
            let name = self.name.clone();
            request(ctx, source, &self.state, move |ctx, transaction| {
                let mut extracted = extracted;
                if extracted.is_none() { if let Some(core::KeyPath::String(path)) = &store.key_path {
                    let generated = transaction.generated_key(&name).map_err(|reason| error(ctx, reason))?;
                    let generated_value = key_value(ctx, &generated);
                    inject_key(ctx, &cloned, path, generated_value)?; extracted = Some(generated);
                } }
                let definitions = transaction.store(&name).map_err(|reason| error(ctx, reason))?.indexes.clone();
                let mut keys = std::collections::BTreeMap::new(); let mut admitted = 0usize;
                for (name, definition) in definitions {
                    let extracted = index_keys(ctx, &cloned, &definition)?;
                    admitted = extracted.iter().fold(admitted, |total, key| total.saturating_add(key.retained_bytes()).saturating_add(96));
                    if admitted > 64 * 1024 * 1024 { return Err(error(ctx, core::Error::QuotaExceeded)); }
                    keys.insert(name, extracted);
                }
                let bytes = crate::structured_clone::serialize_for_storage(ctx, &cloned, 16 * 1024 * 1024)?;
                let result = transaction.put_indexed(&name, bytes, extracted, overwrite, keys).map_err(|e| error(ctx, e))?;
                Ok(key_value(ctx, &result))
            })
        }
    }
    #[methods]
    impl IDBIndex {
        #[getter] fn name(&self) -> String { self.name.clone() }
        #[getter] fn object_store(&self) -> Value { self.object_store.clone() }
        #[getter] fn key_path(&self, ctx: &mut Ctx) -> OpResult<Value> { let state = self.state.core.borrow(); let index = state.database.stores.get(&self.store).and_then(|store| store.indexes.get(&self.name)).ok_or_else(|| error(ctx, core::Error::InvalidState))?; Ok(path_to_value(ctx, &index.key_path)) }
        #[getter] fn unique(&self) -> bool { self.state.core.borrow().database.stores.get(&self.store).and_then(|store| store.indexes.get(&self.name)).is_some_and(|index| index.unique) }
        #[getter] fn multi_entry(&self) -> bool { self.state.core.borrow().database.stores.get(&self.store).and_then(|store| store.indexes.get(&self.name)).is_some_and(|index| index.multi_entry) }
        fn get(&self, ctx: &mut Ctx, this: This<Value>, query: Value) -> OpResult<Value> { self.read(ctx, this.0, query, false) }
        fn get_key(&self, ctx: &mut Ctx, this: This<Value>, query: Value) -> OpResult<Value> { self.read(ctx, this.0, query, true) }
    }
    impl IDBIndex {
        fn read(&self, ctx: &mut Ctx, source: Value, query: Value, primary_only: bool) -> OpResult<Value> {
            ensure_active(ctx, &self.state, false)?;
            let query = range(ctx, &query)?; let store = self.store.clone(); let name = self.name.clone();
            if !self.state.core.borrow().store(&store).map_err(|reason| error(ctx, reason))?.indexes.contains_key(&name) { return Err(error(ctx, core::Error::InvalidState)); }
            request(ctx, source, &self.state, move |ctx, transaction| {
                let result = transaction.index_get(&store, &name, &query).map_err(|reason| error(ctx, reason))?;
                match result { None => Ok(Value::Undefined), Some((key, _)) if primary_only => Ok(key_value(ctx, &key)), Some((_, bytes)) => crate::structured_clone::deserialize_for_storage(ctx, &bytes) }
            })
        }
    }
    #[methods]
    impl IDBKeyRange {
        fn only(ctx: &mut Ctx, value: Value) -> OpResult<Self> {
            let key = key(ctx, &value, 0)?;
            Ok(Self { range: core::KeyRange { lower: Some(key.clone()), upper: Some(key), lower_open: false, upper_open: false } })
        }
        fn lower_bound(ctx: &mut Ctx, value: Value, #[default(false)] open: bool) -> OpResult<Self> { Ok(Self { range: core::KeyRange { lower: Some(key(ctx, &value, 0)?), upper: None, lower_open: open, upper_open: false } }) }
        fn upper_bound(ctx: &mut Ctx, value: Value, #[default(false)] open: bool) -> OpResult<Self> { Ok(Self { range: core::KeyRange { lower: None, upper: Some(key(ctx, &value, 0)?), lower_open: false, upper_open: open } }) }
        fn bound(ctx: &mut Ctx, lower: Value, upper: Value, #[default(false)] lower_open: bool, #[default(false)] upper_open: bool) -> OpResult<Self> {
            let range = core::KeyRange { lower: Some(key(ctx, &lower, 0)?), upper: Some(key(ctx, &upper, 0)?), lower_open, upper_open };
            range.validate().map_err(|e| error(ctx, e))?; Ok(Self { range })
        }
        fn includes(&self, ctx: &mut Ctx, value: Value) -> OpResult<bool> { Ok(self.range.contains(&key(ctx, &value, 0)?)) }
        #[getter] fn lower(&self, ctx: &mut Ctx) -> Value { self.range.lower.as_ref().map_or(Value::Undefined, |key| key_value(ctx, key)) }
        #[getter] fn upper(&self, ctx: &mut Ctx) -> Value { self.range.upper.as_ref().map_or(Value::Undefined, |key| key_value(ctx, key)) }
        #[getter] fn lower_open(&self) -> bool { self.range.lower_open }
        #[getter] fn upper_open(&self) -> bool { self.range.upper_open }
    }
}

fn html_handler(ctx: &mut Ctx, target: &Value, kind: &str, value: Value) -> OpResult<()> {
    let (data, _) = EventTarget::of_receiver(ctx, target)?;
    let callback = lumen::embed::JsFunction::from_value(value).map(events::Callback::Function);
    data.set_handler(kind, callback, events::HandlerKind::Html); Ok(())
}
fn open_database(ctx: &mut Ctx, service: &Rc<Service>, storage_key: &str, name: &str, requested: Option<u64>, target: Value, state: Rc<RequestState>) -> OpResult<()> {
    let snapshot = service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?.load(storage_key, name).map_err(|e| error(ctx, e))?.unwrap_or_default();
    let version = requested.unwrap_or(snapshot.version.max(1));
    if version < snapshot.version {
        *state.error.borrow_mut() = events::dom_exception(ctx, "Requested version is lower than stored version", "VersionError");
        state.done.set(true); event(ctx, &target, "error")?; return Ok(());
    }
    if version > snapshot.version && negotiate(ctx, service, storage_key, name, snapshot.version, Some(version))? {
        service.pending.borrow_mut().push_back(PendingOpen { storage_key: storage_key.into(), name: name.into(), version: requested, target: target.clone(), state, deleting: false });
        version_event(ctx, &target, "blocked", snapshot.version, Some(version))?;
        return Ok(());
    }
    let connection = Rc::new(Connection { service: service.clone(), storage_key: storage_key.into(), name: name.into(), version: Cell::new(version), previous_version: snapshot.version, closed: Cell::new(false), closing: Cell::new(false), transactions: Cell::new(0), upgrade: RefCell::new(None), wrapper: RefCell::new(None), target: TargetData::new(None) });
    service.connections.borrow_mut().push(Rc::downgrade(&connection));
    let database = traced(ctx, bindings::IDBDatabase { base: EventTarget::from_data(connection.target.clone()), connection: connection.clone() })?;
    *connection.wrapper.borrow_mut() = ctx.weak_value(&database);
    *state.result.borrow_mut() = database;
    state.done.set(true);
    if version > snapshot.version {
        let old = snapshot.version; let mut upgraded = snapshot; upgraded.version = version;
        let (transaction, transaction_state) = new_transaction(ctx, &connection, upgraded, vec![], core::Mode::VersionChange)?;
        *state.transaction.borrow_mut() = transaction;
        *state.parent.borrow_mut() = Rc::downgrade(&transaction_state);
        *transaction_state.open.borrow_mut() = Some((target.clone(), state));
        *connection.upgrade.borrow_mut() = Some(transaction_state.clone());
        version_event(ctx, &target, "upgradeneeded", old, Some(version))?;
        if transaction_state.dispatch_failed.replace(false) {
            let reason = events::dom_exception(ctx, "Upgrade event handler threw", "AbortError");
            abort_transaction(ctx, &transaction_state, reason)?;
        }
    } else { event(ctx, &target, "success")?; }
    Ok(())
}

fn negotiate(ctx: &mut Ctx, service: &Rc<Service>, storage_key: &str, name: &str, old: u64, new: Option<u64>) -> OpResult<bool> {
    // Release every registry borrow before invoking author versionchange code.
    let connections: Vec<_> = service.connections.borrow().iter().filter_map(Weak::upgrade).filter(|connection| connection.storage_key == storage_key && connection.name == name && !connection.closed.get()).collect();
    for connection in &connections {
        let target = connection.wrapper.borrow().as_ref().and_then(lumen::embed::WeakValue::upgrade);
        if let Some(target) = target { version_event(ctx, &target, "versionchange", old, new)?; }
        else { connection.closed.set(true); }
    }
    Ok(connections.iter().any(|connection| !connection.closed.get()))
}
fn resume_pending(ctx: &mut Ctx, service: &Rc<Service>) -> OpResult<()> {
    let mut ready = Vec::new();
    {
        let connections = service.connections.borrow();
        let mut pending = service.pending.borrow_mut();
        let mut retained = VecDeque::new();
        while let Some(operation) = pending.pop_front() {
            if connections.iter().filter_map(Weak::upgrade).any(|connection| connection.storage_key == operation.storage_key && connection.name == operation.name && !connection.closed.get()) { retained.push_back(operation); }
            else { ready.push(operation); }
        }
        *pending = retained;
    }
    for operation in ready {
        let service_owned = service.clone();
        queue(ctx, service, move |ctx| {
            if operation.deleting { delete_database(ctx, &service_owned, &operation.storage_key, &operation.name, operation.target, operation.state) }
            else { open_database(ctx, &service_owned, &operation.storage_key, &operation.name, operation.version, operation.target, operation.state) }
        })?;
    }
    Ok(())
}
fn finish_connection(ctx: &mut Ctx, connection: &Rc<Connection>) -> OpResult<()> {
    connection.transactions.set(connection.transactions.get().saturating_sub(1));
    if connection.transactions.get() == 0 && connection.closing.get() {
        connection.closed.set(true);
        resume_pending(ctx, &connection.service)?;
    }
    Ok(())
}
fn delete_database(ctx: &mut Ctx, service: &Rc<Service>, storage_key: &str, name: &str, target: Value, state: Rc<RequestState>) -> OpResult<()> {
    let database = service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?.load(storage_key, name).map_err(|e| error(ctx, e))?;
    let old = database.as_ref().map_or(0, |db| db.version);
    if negotiate(ctx, service, storage_key, name, old, None)? {
        service.pending.borrow_mut().push_back(PendingOpen { storage_key: storage_key.into(), name: name.into(), version: None, target: target.clone(), state, deleting: true });
        version_event(ctx, &target, "blocked", old, None)?; return Ok(());
    }
    let result = service.environment.backend.lock().map_err(|_| OpError::new("UnknownError", "IndexedDB backend lock poisoned"))?.replace(storage_key, name, database.map_or(0, |db| db.revision), None);
    state.done.set(true);
    if let Err(reason) = result { *state.error.borrow_mut() = events::dom_exception(ctx, reason.name(), reason.name()); event(ctx, &target, "error")?; }
    else { version_event(ctx, &target, "success", old, None)?; }
    Ok(())
}

/// HTML/worker task cleanup calls this after its microtask checkpoint. A
/// transaction is active during its own request callbacks and their microtasks,
/// never throughout unrelated future task callbacks.
pub fn end_task(ctx: &mut Ctx) {
    // Track only transactions actually activated in this task/checkpoint. Jobs
    // can run in child realms before the engine restores its root realm; using
    // the current realm here would leave those transactions spuriously active.
    let active = ctx.op_state().get_mut::<ActiveTransactions>().map(|active| std::mem::take(&mut active.0)).unwrap_or_default();
    for transaction in active {
        if let Some(transaction) = transaction.upgrade() { transaction.active.set(false); }
    }
}
fn track_active(ctx: &mut Ctx, transaction: &Rc<TransactionState>) {
    if !ctx.op_state().has::<ActiveTransactions>() { ctx.op_state().put(ActiveTransactions::default()); }
    ctx.op_state().get_mut::<ActiveTransactions>().unwrap().0.push(Rc::downgrade(transaction));
}

impl lumen::embed::NativeIdentityOwner for bindings::IDBFactory {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        for operation in self.service.pending.borrow().iter() { visit(&operation.target); operation.state.trace(visit); }
    }
}

impl lumen::embed::NativeIdentityOwner for bindings::IDBRequest {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.base.data_handle().trace_callbacks(visit); self.state.trace(visit); }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBOpenDBRequest {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.base.trace_native_values(visit); }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBDatabase {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.base.data_handle().trace_callbacks(visit); if let Some(state) = self.connection.upgrade.borrow().as_ref() { visit(&state.wrapper.borrow()); } }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBTransaction {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.base.data_handle().trace_callbacks(visit); visit(&self.state.wrapper.borrow()); visit(&self.state.database); visit(&self.state.error.borrow()); if let Some((request, state)) = self.state.open.borrow().as_ref() { visit(request); state.trace(visit); } }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBObjectStore {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { visit(&self.state.wrapper.borrow()); }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBIndex {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { visit(&self.object_store); visit(&self.state.wrapper.borrow()); }
}
impl lumen::embed::NativeIdentityOwner for bindings::IDBVersionChangeEvent {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.base.trace_native_values(visit); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_db_native_async_vertical_slice() {
        std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
            let mut engine = lumen::Engine::new();
            assert!(crate::globals::<events::bindings::Module>(engine.ctx()).is_ok(), "event bindings installation");
            let tasks: Rc<RefCell<VecDeque<Task>>> = Rc::new(RefCell::new(VecDeque::new()));
            let admission = tasks.clone();
            install(engine.ctx(), Environment {
                storage_key: Rc::new(|_| Ok("https://db.test".into())),
                backend: Arc::new(Mutex::new(core::MemoryBackend::new(16, 1024 * 1024))),
                queue: Rc::new(move |_, task| { if admission.borrow().len() >= 256 { return Err(OpError::new("QuotaExceededError", "Test queue full")); } admission.borrow_mut().push_back(task); Ok(()) }),
            }).unwrap();
            fn eval(engine: &mut lumen::Engine, script: &str) -> Value {
                match engine.eval_value(script).unwrap() {
                    Ok(value) => value,
                    Err(value) => { let text = engine.ctx().coerce_string(&value).unwrap_or_default(); panic!("Native IDB script failed: {text}"); }
                }
            }
            eval(&mut engine, r#"
                globalThis.done=false;globalThis.log=[];
                const opening=indexedDB.open('native',1);
                if(opening.readyState!=='pending')throw new Error('synchronous completion');
                opening.onupgradeneeded=e=>{
                    if(e.oldVersion!==0||e.newVersion!==1||!e.isTrusted)throw new Error('upgrade event');
                    log.push('upgrade');
                    const tx=opening.transaction,store=opening.result.createObjectStore('store');
                    const payload={answer:42};const put=store.put(payload,1);payload.answer=0;
                    put.onsuccess=()=>{
                        if(put.result!==1)throw new Error('put key');log.push('put');
                        Promise.resolve().then(()=>{
                            store.get(1).onsuccess=e=>{if(e.target.result.answer!==42)throw new Error('clone snapshot');log.push('get');};
                        });
                    };
                    tx.oncomplete=()=>log.push('complete');
                };
                opening.onsuccess=()=>{if(opening.transaction!==null)throw new Error('transaction not cleared');opening.result.close();log.push('success');done=true;};
            "#);
            assert!(matches!(eval(&mut engine, "!done"), Value::Bool(true)));
            while engine.run_one_job() {}
            end_task(engine.ctx());
            for _ in 0..256 {
                let next = tasks.borrow_mut().pop_front();
                let Some(task) = next else { break; };
                if let Err(failure) = task(engine.ctx()) { let value = failure.to_value(engine.ctx()); let message = engine.ctx().coerce_string(&value).unwrap_or_default(); panic!("IDB task failed: {message}"); }
                while engine.run_one_job() {}
                end_task(engine.ctx());
            }
            assert!(matches!(eval(&mut engine, "done && log.join(',')==='upgrade,put,get,complete,success'"), Value::Bool(true)), "native asynchronous transaction lifecycle");
        }).unwrap().join().unwrap();
    }
}
