//! Web Storage: `window.localStorage`, `window.sessionStorage`, `Storage` and `StorageEvent`.
//!
//! The algorithms and the 5 MiB per-origin quota live in `lumen_common::web_storage`. `localStorage`
//! reads and writes through a host-supplied [`SharedBackend`] (in-memory unless the embedder calls
//! [`set_local_storage_backend`]); `sessionStorage` is an in-memory backend owned by the
//! document's top-level traversable (the interpreter owns one for documents without a browsing
//! context). A `Storage` object is the Web IDL named-property Proxy of [`crate::dataset`]
//! around a [`DomStorageState`], so `storage.key`, `storage[key] = value` and `delete storage.key`
//! reach the same operations as the methods.
use super::*;
use crate::events::DomEvent;
use lumen::embed::JsHost;
use lumen_bind::Host;
use lumen_common::web_storage::{MemoryBackend, QuotaExceeded, SharedBackend};
use lumen_host::events::EventInit;

const HANDLE_SLOT: &str = "#lumen_storage\u{1}state";
const LOCAL_SLOT: &str = "#lumen_window\u{1}localStorage";
const SESSION_SLOT: &str = "#lumen_window\u{1}sessionStorage";

#[derive(Clone)]
struct LocalBackend(SharedBackend);

#[derive(Clone)]
struct SessionBackend(SharedBackend);

/// Supply the `localStorage` backend of this interpreter's documents before installing them. The
/// default is bounded memory storage that is lost with the interpreter.
pub fn set_local_storage_backend(ctx: &mut Ctx, backend: SharedBackend) {
    ctx.op_state().put(LocalBackend(backend));
}

fn backend_for(ctx: &mut Ctx, session: bool) -> SharedBackend {
    if session {
        if let Some(backend) = ctx.op_state().get::<SessionBackend>() {
            return backend.0.clone();
        }
        let backend = MemoryBackend::shared();
        ctx.op_state().put(SessionBackend(backend.clone()));
        backend
    } else {
        if let Some(backend) = ctx.op_state().get::<LocalBackend>() {
            return backend.0.clone();
        }
        let backend = MemoryBackend::shared();
        set_local_storage_backend(ctx, backend.clone());
        backend
    }
}

/// The key under which a document's storage lives: its serialized origin. `file:` documents have
/// an opaque origin but share one storage key, like Chrome.
fn storage_key(realm: &DomRealm) -> Option<String> {
    let origin = realm.document_origin()?.serialize();
    if origin != "null" {
        return Some(origin);
    }
    let url = realm.document_url()?;
    url.get(..5).is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:")).then(|| "file://".to_owned())
}

/// The other active documents that share the storage area `source` wrote: the same origin for
/// `localStorage`, additionally the same top-level traversable for `sessionStorage`.
fn recipients(source: &Rc<DomRealm>, origin: &str, session: bool) -> Vec<(Rc<DomRealm>, lumen::embed::RealmHandle)> {
    let Some(context) = source.browsing_context() else { return Vec::new() };
    let top = session.then(|| context.top_context());
    context
        .group_documents()
        .into_iter()
        .filter(|(document, _)| {
            if Rc::ptr_eq(document, source) || storage_key(document).as_deref() != Some(origin) {
                return false;
            }
            match &top {
                None => true,
                Some(top) => document.browsing_context().is_some_and(|other| Rc::ptr_eq(&other.top_context(), top)),
            }
        })
        .collect()
}

fn quota_error() -> OpError {
    OpError::new("QuotaExceededError", "The storage quota for this origin is exceeded")
}

fn illegal_invocation() -> OpError {
    OpError::type_error("Illegal invocation")
}

/// The data behind one `Storage` object.
#[derive(Clone)]
pub(crate) struct StorageState {
    realm: std::rc::Weak<DomRealm>,
    origin: String,
    session: bool,
    backend: SharedBackend,
}

impl StorageState {
    fn origin(&self) -> String {
        self.realm.upgrade().and_then(|realm| storage_key(&realm)).unwrap_or_else(|| self.origin.clone())
    }

    fn with<R>(&self, f: impl FnOnce(&mut dyn lumen_common::web_storage::StorageBackend, &str) -> R) -> R {
        let origin = self.origin();
        let mut backend = self.backend.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut *backend, &origin)
    }

    fn length(&self) -> usize {
        self.with(|backend, origin| backend.len(origin))
    }

    fn key(&self, index: usize) -> Option<String> {
        self.with(|backend, origin| backend.key(origin, index))
    }

    fn get(&self, key: &str) -> Option<String> {
        self.with(|backend, origin| backend.get_item(origin, key))
    }

    fn keys(&self) -> Vec<String> {
        self.with(|backend, origin| backend.keys(origin))
    }

    fn set(&self, ctx: &mut Ctx, key: &str, value: &str) -> OpResult<()> {
        if self.with(|backend, origin| backend.item_equals(origin, key, value)) {
            return Ok(());
        }
        let old = self
            .with(|backend, origin| backend.set_item(origin, key, value))
            .map_err(|QuotaExceeded| quota_error())?;
        self.notify(ctx, Some(key), old, Some(value.to_owned()));
        Ok(())
    }

    fn has(&self, key: &str) -> bool {
        self.with(|backend, origin| backend.contains(origin, key))
    }

    fn remove(&self, ctx: &mut Ctx, key: &str) -> OpResult<()> {
        match self.with(|backend, origin| backend.remove_item(origin, key)) {
            Some(old) => self.notify(ctx, Some(key), Some(old), None),
            None => {}
        }
        Ok(())
    }

    fn clear(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.with(|backend, origin| backend.clear(origin)) {
            self.notify(ctx, None, None, None);
        }
        Ok(())
    }

    /// Queues the `storage` event for every other document that shares this area. The write has
    /// already committed, so a full task queue only drops the event; nothing is queued when no
    /// other document shares the area.
    fn notify(&self, ctx: &mut Ctx, key: Option<&str>, old: Option<String>, new: Option<String>) {
        let Some(source) = self.realm.upgrade() else { return };
        let Some(url) = source.document_url() else { return };
        let origin = self.origin();
        let session = self.session;
        if recipients(&source, &origin, session).is_empty() {
            return;
        }
        let weak = Rc::downgrade(&source);
        let key = key.map(str::to_owned);
        let _ = scheduling::queue_task(ctx, move |ctx| {
            let Some(source) = weak.upgrade() else { return Ok(()) };
            for (document, handle) in recipients(&source, &origin, session) {
                let (key, old, new, url) = (key.clone(), old.clone(), new.clone(), url.clone());
                let delivered = ctx.with_host_realm(&handle, |ctx| {
                    deliver_event(ctx, &document, session, key, old, new, url)
                });
                if let Ok(Err(error)) = delivered {
                    let error = error.to_value(ctx);
                    DomRealm::report_exception(ctx, error);
                }
            }
            Ok(())
        });
    }
}

fn deliver_event(
    ctx: &mut Ctx,
    document: &Rc<DomRealm>,
    session: bool,
    key: Option<String>,
    old: Option<String>,
    new: Option<String>,
    url: String,
) -> OpResult<()> {
    let Some(window) = document.window_wrapper.borrow().as_ref().and_then(WeakValue::upgrade) else {
        return Ok(());
    };
    let area = match window_storage(ctx, Some(document), session) {
        Ok(area) => area,
        Err(_) => return Ok(()),
    };
    let event = ctx.new_instance(DomStorageEvent {
        base: DomEvent::from_init("storage", EventInit::default()),
        fields: RefCell::new(StorageEventFields { key, old_value: old, new_value: new, url, storage_area: area }),
    });
    ctx.set_native_identity_owner::<DomStorageEvent>(&event)?;
    if let Some(event) = JsObject::from_value(event) {
        events::dispatch_user_agent_event(ctx, lumen_bind::This(window), event)?;
    }
    Ok(())
}

fn state_of(ctx: &mut Ctx, receiver: &Value) -> OpResult<StorageState> {
    let handle = ctx.native_private_value_slot(receiver, HANDLE_SLOT).ok_or_else(illegal_invocation)?;
    ctx.with_instance::<DomStorageHandle, _>(&handle, |handle| handle.state.clone())
}

/// `window.localStorage` / `window.sessionStorage` for `realm`'s window.
pub(crate) fn window_storage(ctx: &mut Ctx, realm: Option<&Rc<DomRealm>>, session: bool) -> OpResult<Value> {
    let realm = realm.ok_or_else(|| OpError::new("SecurityError", "The document has no storage"))?;
    let origin = storage_key(realm)
        .ok_or_else(|| OpError::new("SecurityError", "Storage is unavailable for an opaque origin"))?;
    let handle = realm
        .relevant_host_realm(ctx)
        .ok_or_else(|| OpError::new("SecurityError", "The document has no storage"))?;
    let slot = if session { SESSION_SLOT } else { LOCAL_SLOT };
    if let Some(existing) = ctx.native_private_value_slot(&handle.global(), slot) {
        return Ok(existing);
    }
    let backend = match realm.browsing_context() {
        Some(context) if session => context.session_storage_backend(),
        _ => backend_for(ctx, session),
    };
    let state = StorageState { realm: Rc::downgrade(realm), origin, session, backend };
    let created = ctx
        .with_host_realm(&handle, |ctx| create_storage(ctx, realm, state))
        .map_err(|_| OpError::new("SecurityError", "The document has no storage"))??;
    ctx.set_native_internal_value_slot(&handle.global(), slot, created.clone()).map_err(OpError::thrown)?;
    Ok(created)
}

fn create_storage(ctx: &mut Ctx, realm: &Rc<DomRealm>, state: StorageState) -> OpResult<Value> {
    let intrinsics = dataset::intrinsics(realm)?;
    let constructor = ctx.class_constructor::<DomStorage>();
    let prototype = ctx.member_get(&constructor, "prototype").map_err(OpError::thrown)?;
    let handle = ctx.new_instance(DomStorageHandle { state });
    let proxy = dataset::build_proxy(ctx, &intrinsics, &handle, Some(&prototype))?;
    ctx.set_native_internal_value_slot(&proxy, HANDLE_SLOT, handle).map_err(OpError::thrown)?;
    Ok(proxy)
}

#[lumen_bind::class(name = "Storage", hint(js(webidl)))]
pub(crate) struct DomStorage {}

#[lumen_bind::methods]
impl DomStorage {
    #[getter]
    fn length(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<usize> {
        Ok(state_of(ctx, &this.0)?.length())
    }

    #[method(coerce)]
    fn key(ctx: &mut Ctx, this: lumen_bind::This<Value>, index: u32) -> OpResult<Nullable<String>> {
        Ok(Nullable(state_of(ctx, &this.0)?.key(index as usize)))
    }

    #[method(name = "getItem", coerce)]
    fn get_item(ctx: &mut Ctx, this: lumen_bind::This<Value>, key: &str) -> OpResult<Nullable<String>> {
        Ok(Nullable(state_of(ctx, &this.0)?.get(key)))
    }

    #[method(name = "setItem", coerce)]
    fn set_item(ctx: &mut Ctx, this: lumen_bind::This<Value>, key: &str, value: &str) -> OpResult<()> {
        state_of(ctx, &this.0)?.set(ctx, key, value)
    }

    #[method(name = "removeItem", coerce)]
    fn remove_item(ctx: &mut Ctx, this: lumen_bind::This<Value>, key: &str) -> OpResult<()> {
        state_of(ctx, &this.0)?.remove(ctx, key)
    }

    #[method]
    fn clear(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        state_of(ctx, &this.0)?.clear(ctx)
    }
}

#[lumen_bind::class(name = "StorageNamedProperties")]
struct DomStorageHandle {
    state: StorageState,
}

#[lumen_bind::methods]
impl DomStorageHandle {
    #[method(name = "hasNamedProperty", coerce)]
    fn has_named_property(&self, name: &str) -> bool {
        self.state.has(name)
    }

    #[method(name = "getNamedProperty", coerce)]
    fn get_named_property(&self, name: &str) -> Nullable<String> {
        Nullable(self.state.get(name))
    }

    #[method(name = "setNamedProperty", coerce)]
    fn set_named_property(&self, ctx: &mut Ctx, name: &str, value: &str) -> OpResult<()> {
        self.state.set(ctx, name, value)
    }

    #[method(name = "deleteNamedProperty", coerce)]
    fn delete_named_property(&self, ctx: &mut Ctx, name: &str) -> OpResult<()> {
        self.state.remove(ctx, name)
    }

    #[method(name = "supportedPropertyNames")]
    fn supported_property_names(&self) -> Vec<String> {
        self.state.keys()
    }
}

struct StorageEventFields {
    key: Option<String>,
    old_value: Option<String>,
    new_value: Option<String>,
    url: String,
    storage_area: Value,
}

#[lumen_bind::class(name = "StorageEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomStorageEvent {
    base: DomEvent,
    fields: RefCell<StorageEventFields>,
}

impl lumen::embed::NativeIdentityOwner for DomStorageEvent {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, id: u64, visit: &mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_identities(&self.base, id, visit);
    }
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_values(&self.base, visit);
        visit(&self.fields.borrow().storage_area);
    }
}

struct StorageEventConstructor(DomStorageEvent);

impl lumen_bind::CtorRet<JsHost, DomStorageEvent> for StorageEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let value = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx| {
            ctx.set_native_identity_owner::<DomStorageEvent>(&value).expect("StorageEvent native brand")
        });
        Ok(value)
    }
}

fn optional_string(ctx: &mut Ctx, value: Option<Value>) -> OpResult<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string())),
    }
}

fn dictionary_string(ctx: &mut Ctx, options: &Option<Value>, name: &str) -> OpResult<Option<String>> {
    let member = ui_events::dictionary_member(ctx, options, name)?;
    optional_string(ctx, member)
}

fn storage_area_arg(ctx: &mut Ctx, value: Option<Value>) -> OpResult<Value> {
    match value {
        None | Some(Value::Null | Value::Undefined) => Ok(Value::Null),
        Some(value) => {
            if ctx.native_private_value_slot(&value, HANDLE_SLOT).is_none() {
                return Err(OpError::type_error("StorageEvent storageArea must be a Storage object"));
            }
            Ok(value)
        }
    }
}

#[lumen_bind::methods]
impl DomStorageEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<StorageEventConstructor> {
        let init = EventInit::read(ctx, &options)?;
        let key = dictionary_string(ctx, &options, "key")?;
        let old_value = dictionary_string(ctx, &options, "oldValue")?;
        let new_value = dictionary_string(ctx, &options, "newValue")?;
        let url = match ui_events::dictionary_member(ctx, &options, "url")? {
            Some(value) => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(),
            None => String::new(),
        };
        let area = ui_events::dictionary_member(ctx, &options, "storageArea")?;
        let storage_area = storage_area_arg(ctx, area)?;
        Ok(StorageEventConstructor(Self {
            base: DomEvent::from_init(kind, init),
            fields: RefCell::new(StorageEventFields { key, old_value, new_value, url, storage_area }),
        }))
    }

    #[getter]
    fn key(&self) -> Nullable<String> {
        Nullable(self.fields.borrow().key.clone())
    }

    #[getter(name = "oldValue")]
    fn old_value(&self) -> Nullable<String> {
        Nullable(self.fields.borrow().old_value.clone())
    }

    #[getter(name = "newValue")]
    fn new_value(&self) -> Nullable<String> {
        Nullable(self.fields.borrow().new_value.clone())
    }

    #[getter]
    fn url(&self) -> String {
        self.fields.borrow().url.clone()
    }

    #[getter(name = "storageArea")]
    fn storage_area(&self) -> Value {
        self.fields.borrow().storage_area.clone()
    }

    #[method(name = "initStorageEvent", coerce)]
    fn init_storage_event(
        &self,
        ctx: &mut Ctx,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
        key: Option<Value>,
        old_value: Option<Value>,
        new_value: Option<Value>,
        #[default(String::new())] url: String,
        storage_area: Option<Value>,
    ) -> OpResult<()> {
        let key = optional_string(ctx, key)?;
        let old_value = optional_string(ctx, old_value)?;
        let new_value = optional_string(ctx, new_value)?;
        let storage_area = storage_area_arg(ctx, storage_area)?;
        if self.base.initialize_legacy(kind, bubbles, cancelable) {
            *self.fields.borrow_mut() = StorageEventFields { key, old_value, new_value, url, storage_area };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let stack = engine.ctx().get_member(&error, "stack").ok().and_then(|stack| engine.ctx().coerce_string(&stack).ok());
                panic!("{stack:?}\nSource: {source}");
            }
        }
    }

    fn engine_for(url: &str) -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<body></body>", 256).unwrap();
        realm.set_document_url(url);
        (engine, realm)
    }

    fn check(engine: &mut Engine, source: &str) {
        let wrapped = format!("(()=>{{{source}}})()");
        assert!(matches!(eval(engine, &wrapped), Value::Bool(true)), "failed: {source}");
    }

    #[test]
    fn storage_is_a_named_property_interface_over_the_origin_area() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const s = localStorage;
            if (!('localStorage' in window) || !('sessionStorage' in window)) return false;
            if (!(s instanceof Storage) || s !== window.localStorage || s === sessionStorage) return false;
            if (Object.prototype.toString.call(s) !== '[object Storage]') return false;
            if (s.length !== 0 || s.key(0) !== null || s.getItem('a') !== null || s.a !== undefined) return false;
            s.setItem('a', 1); s.b = 'two'; s[3] = null;
            if (s.length !== 3 || s.getItem('a') !== '1' || s.a !== '1' || s.b !== 'two' || s['3'] !== 'null') return false;
            if (s.key(0) !== 'a' || s.key(1) !== 'b' || s.key(2) !== '3' || s.key(3) !== null) return false;
            if (JSON.stringify(Object.keys(s)) !== '["a","b","3"]' || !('a' in s) || ('zz' in s)) return false;
            if (JSON.stringify(s) !== '{"3":"null","a":"1","b":"two"}' && JSON.stringify(s) !== '{"a":"1","b":"two","3":"null"}') return false;
            const seen = []; for (const key in s) if (Object.prototype.hasOwnProperty.call(s, key)) seen.push(key);
            if (seen.join() !== 'a,b,3') return false;
            delete s.a; s.removeItem('b'); s.removeItem('missing');
            if (s.length !== 1 || s.a !== undefined || s.getItem('b') !== null) return false;
            s.clear();
            return s.length === 0 && Object.keys(s).length === 0;
        "#);
    }

    #[test]
    fn built_in_members_win_over_stored_names_but_can_be_stored() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const s = localStorage;
            s.getItem = 'shadow'; s.length = '9'; s.toString = 'x';
            if (typeof s.getItem !== 'function' || s.length !== 3) return false;
            if (s.constructor !== Storage || s.getItem('getItem') !== 'shadow' || s.key(0) !== 'getItem') return false;
            if (Storage.prototype.getItem.call(s, 'length') !== '9') return false;
            delete s.getItem;
            return s.getItem('getItem') === 'shadow' && s.length === 3;
        "#);
    }

    #[test]
    fn methods_validate_receiver_and_arguments_and_convert_to_strings() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const throwsType = f => { try { f(); } catch (e) { return e instanceof TypeError; } return false; };
            if (!throwsType(() => Storage.prototype.getItem.call({}, 'a'))) return false;
            if (!throwsType(() => Storage.prototype.clear.call(Storage.prototype))) return false;
            if (!throwsType(() => new Storage())) return false;
            if (!throwsType(() => localStorage.getItem())) return false;
            if (!throwsType(() => localStorage.setItem('only-key'))) return false;
            if (!throwsType(() => localStorage.setItem(Symbol(), 'v'))) return false;
            localStorage.setItem({ toString() { return 'obj'; } }, undefined);
            if (localStorage.getItem('obj') !== 'undefined') return false;
            return Storage.prototype.getItem.call(sessionStorage, 'obj') === null;
        "#);
    }

    #[test]
    fn the_quota_throws_quota_exceeded_and_keeps_existing_data() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const big = 'x'.repeat(1024 * 1024);
            let stored = 0, error = null;
            try { for (let i = 0; i < 8; i++) { localStorage.setItem('k' + i, big); stored++; } } catch (e) { error = e; }
            if (!error || error.name !== 'QuotaExceededError' || !(error instanceof DOMException)) return false;
            if (stored < 2 || localStorage.length !== stored || localStorage.getItem('k0') !== big) return false;
            let viaProperty = null;
            try { localStorage.late = big; } catch (e) { viaProperty = e; }
            if (!viaProperty || viaProperty.name !== 'QuotaExceededError') return false;
            localStorage.k0 = 'small';
            localStorage.clear();
            sessionStorage.setItem('independent', 'ok');
            return localStorage.length === 0 && sessionStorage.length === 1;
        "#);
    }

    #[test]
    fn opaque_origins_throw_security_error_from_the_getters() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        check(&mut engine, r#"
            let name = '';
            try { localStorage; } catch (e) { name = e.name; }
            let other = '';
            try { window.sessionStorage; } catch (e) { other = e.name; }
            return name === 'SecurityError' && other === 'SecurityError' && ('localStorage' in window);
        "#);
    }

    #[test]
    fn storage_events_validate_dictionaries_and_expose_the_attributes() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const empty = new StorageEvent('storage');
            if (empty.key !== null || empty.oldValue !== null || empty.newValue !== null || empty.url !== '' || empty.storageArea !== null) return false;
            if (!(empty instanceof Event) || empty.bubbles || empty.cancelable) return false;
            const e = new StorageEvent('storage', { key: 'k', oldValue: 1, newValue: null, url: 'https://x.test/', storageArea: localStorage, bubbles: true });
            if (e.key !== 'k' || e.oldValue !== '1' || e.newValue !== null || e.url !== 'https://x.test/') return false;
            if (e.storageArea !== localStorage || !e.bubbles) return false;
            let rejected = false;
            try { new StorageEvent('storage', { storageArea: {} }); } catch (error) { rejected = error instanceof TypeError; }
            if (!rejected) return false;
            const legacy = new StorageEvent('x');
            legacy.initStorageEvent('storage', true, true, 'a', 'b', 'c', 'u', sessionStorage);
            return legacy.type === 'storage' && legacy.key === 'a' && legacy.oldValue === 'b' && legacy.newValue === 'c'
                && legacy.url === 'u' && legacy.storageArea === sessionStorage && legacy.cancelable;
        "#);
    }

    #[test]
    fn a_host_backend_receives_local_storage_writes_but_not_session_storage() {
        let backend = MemoryBackend::shared();
        let mut engine = Engine::new();
        set_local_storage_backend(engine.ctx(), backend.clone());
        let realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("https://host.test/");
        eval(&mut engine, "localStorage.setItem('persisted', 'yes'); sessionStorage.setItem('volatile', 'no'); 0");
        let guard = backend.lock().unwrap();
        assert_eq!(guard.get_item("https://host.test", "persisted").as_deref(), Some("yes"));
        assert_eq!(guard.get_item("https://host.test", "volatile"), None);
    }

    fn parent_with_child(parent_url: &str, child_url: &str) -> (Engine, Rc<DomRealm>, Rc<DomRealm>) {
        let (mut engine, parent) = engine_for(parent_url);
        eval(&mut engine, "const frame = document.createElement('iframe'); frame.id = 'child'; document.body.appendChild(frame); 0");
        let node = {
            let session = parent.session.borrow();
            lumen_html::selector::get_element_by_id(session.document(), session.document().root(), "child").unwrap().unwrap()
        };
        let frame = parent.ensure_frame_context(engine.ctx(), node).unwrap();
        let child = frame.current_document().unwrap();
        child.set_document_url(child_url);
        (engine, parent, child)
    }

    #[test]
    fn storage_events_reach_other_same_origin_documents_but_not_the_writer() {
        let (mut engine, _parent, _child) = parent_with_child("https://app.test/index.html", "https://app.test/child.html");
        eval(&mut engine, r#"
            globalThis.log = [];
            const child = document.getElementById('child').contentWindow;
            child.addEventListener('storage', e => log.push(['child', e.key, e.oldValue, e.newValue, e.url, e.storageArea === child.localStorage, e.isTrusted].join()));
            window.addEventListener('storage', e => log.push('parent-saw-own-write'));
            localStorage.setItem('k', 'v1');
            localStorage.setItem('k', 'v1');
            localStorage.setItem('k', 'v2');
            localStorage.removeItem('missing');
            localStorage.removeItem('k');
            localStorage.setItem('other', 'x');
            localStorage.clear();
            localStorage.clear();
            sessionStorage.setItem('s', '1');
            0
        "#);
        assert!(crate::scheduling::run_tasks(&mut engine, 64).is_empty());
        let url = "https://app.test/index.html";
        check(&mut engine, &format!(r#"
            const expected = [
                'child,k,,v1,{url},true,true', 'child,k,v1,v2,{url},true,true', 'child,k,v2,,{url},true,true',
                'child,other,,x,{url},true,true', 'child,,,,{url},true,true',
                'child,s,,1,{url},false,true',
            ];
            return log.join('|') === expected.join('|');
        "#));
    }

    #[test]
    fn storage_events_skip_documents_of_other_origins() {
        let (mut engine, _parent, _child) = parent_with_child("https://app.test/index.html", "https://elsewhere.test/child.html");
        eval(&mut engine, r#"
            globalThis.log = [];
            const child = document.getElementById('child').contentWindow;
            localStorage.setItem('k', 'v');
            0
        "#);
        assert!(crate::scheduling::run_tasks(&mut engine, 64).is_empty());
        check(&mut engine, "return log.length === 0 && (() => { try { document.getElementById('child').contentWindow.localStorage; return false; } catch (e) { return e.name === 'SecurityError'; } })();");
    }

    #[test]
    fn writes_commit_and_never_throw_because_of_event_queueing() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            for (let i = 0; i < 6000; i++) localStorage.setItem('k' + i, 'v');
            return localStorage.length === 6000;
        "#);
        assert!(crate::scheduling::run_tasks(&mut engine, 64).is_empty());
        let (mut engine, _parent, _child) = parent_with_child("https://app.test/index.html", "https://app.test/child.html");
        check(&mut engine, r#"
            for (let i = 0; i < 6000; i++) localStorage.setItem('k' + i, 'v');
            return localStorage.length === 6000 && localStorage.getItem('k5999') === 'v';
        "#);
    }

    #[test]
    fn session_storage_belongs_to_the_top_level_traversable() {
        let (mut engine, parent, _child) = parent_with_child("https://app.test/index.html", "https://app.test/child.html");
        let aux = parent.create_auxiliary_context(engine.ctx(), String::new()).unwrap();
        aux.document().unwrap().set_document_url("https://app.test/aux.html");
        let proxy = aux.proxy().unwrap();
        let global = engine.ctx().global_this();
        engine.ctx().member_set(&global, "aux", proxy).ok().unwrap();
        eval(&mut engine, r#"
            globalThis.log = [];
            const child = document.getElementById('child').contentWindow;
            child.addEventListener('storage', e => log.push('child:' + e.key + ':' + (e.storageArea === child.sessionStorage ? 'session' : 'local')));
            aux.addEventListener('storage', e => log.push('aux:' + e.key + ':' + (e.storageArea === aux.sessionStorage ? 'session' : 'local')));
            sessionStorage.setItem('s', '1');
            localStorage.setItem('l', '1');
            0
        "#);
        assert!(crate::scheduling::run_tasks(&mut engine, 64).is_empty());
        check(&mut engine, r#"
            const child = document.getElementById('child').contentWindow;
            return child.sessionStorage.getItem('s') === '1' && aux.sessionStorage.getItem('s') === null && aux.sessionStorage.length === 0
                && aux.localStorage.getItem('l') === '1'
                && log.slice().sort().join() === 'aux:l:local,child:l:local,child:s:session';
        "#);
    }

    #[test]
    fn file_documents_share_one_storage_key() {
        let backend = MemoryBackend::shared();
        let mut engine = Engine::new();
        set_local_storage_backend(engine.ctx(), backend.clone());
        let realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("file:///home/user/page.html");
        eval(&mut engine, "localStorage.setItem('f', 'yes'); 0");
        assert_eq!(backend.lock().unwrap().get_item("file://", "f").as_deref(), Some("yes"));
        check(&mut engine, "return sessionStorage.length === 0 && localStorage.getItem('f') === 'yes';");
    }

    #[test]
    fn named_property_set_only_applies_when_the_receiver_is_the_storage() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            const derived = Object.create(localStorage);
            derived.own = 'x';
            if (localStorage.getItem('own') !== null || !Object.prototype.hasOwnProperty.call(derived, 'own')) return false;
            Reflect.set(localStorage, 'viaReflect', 'y', localStorage);
            if (localStorage.getItem('viaReflect') !== 'y') return false;
            localStorage.setItem('present', '1');
            return ('present' in localStorage) && ('present' in derived) && !('absent' in localStorage);
        "#);
    }

    #[test]
    fn large_areas_enumerate_beyond_the_old_key_list_cap() {
        let (mut engine, _realm) = engine_for("https://storage.test/page");
        check(&mut engine, r#"
            for (let i = 0; i < 70000; i++) localStorage.setItem('k' + i, '');
            return Object.keys(localStorage).length === 70000 && localStorage.key(69999) === 'k69999';
        "#);
    }
}
