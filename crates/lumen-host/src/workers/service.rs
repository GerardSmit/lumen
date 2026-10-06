//! `ServiceWorker`, `ServiceWorkerRegistration` and `ServiceWorkerContainer` over a host's
//! [`ServiceWorkerRegistry`].
//!
//! A realm that wants `navigator.serviceWorker` calls [`install_service_workers`]. That defines
//! the accessor on the realm's own `navigator` object (the `Navigator` prototype every realm
//! shares is untouched, so `'serviceWorker' in navigator` is true only where a registry exists)
//! and publishes the three interfaces as lazy globals. Nothing else happens until a script reads
//! `navigator.serviceWorker`: the container, its [`Control`] and its loop task are created then.
//!
//! State is pushed, never polled. The registry sends [`WorkerEvent`]s to the container's
//! control; one wake drains a bounded batch, and the control's loop task is unref'd and idle
//! until something arrives. A [`Space`] holds what a realm needs to apply them: the wrappers
//! cached per worker and registration id (weakly: a wrapper nobody references is collected and
//! rebuilt from the next record), the pending `register()`/`update()` jobs and `ready`. A wrapper
//! with listeners is pinned only while an event can still reach it.

use super::control::{Control, WorkerEvent};
use super::page::{dom_error, page_location};
use super::registry::{
    is_secure_context, ClientInfo, RegisterRequest, RegistrationRecord, ServiceWorkerRegistry,
    UpdateViaCache, WorkerRecord, WorkerState,
};
use crate::events::{
    node_handler_get, node_handler_set, same, Event, EventTarget, TargetData,
};
use crate::messaging::{deserialize_message, serialize_message, MessageEvent};
use crate::webidl::usv_string;
use crate::{lazy_globals, TaskId, TaskRegistry};
use lumen::embed::{
    Ctx, Deferred, NativeIdentityOwner, OpError, OpResult, Promise, Value, WeakValue,
};
use lumen_bind::{Passed, This};
use lumen_os::channel::Pop;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// How many pushed events one wake applies before yielding to the loop.
const DRAIN_BATCH: usize = 64;

/// Weak wrapper identity by key. Dead entries are swept when the map doubles.
pub(super) struct WeakMap<K> {
    map: HashMap<K, WeakValue>,
    limit: usize,
}

impl<K> Default for WeakMap<K> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            limit: 0,
        }
    }
}

impl<K: std::hash::Hash + Eq> WeakMap<K> {
    pub(super) fn get(&self, key: &K) -> Option<Value> {
        self.map.get(key).and_then(WeakValue::upgrade)
    }

    pub(super) fn insert(&mut self, key: K, weak: WeakValue) {
        if self.map.len() >= self.limit.max(32) {
            self.map.retain(|_, entry| entry.upgrade().is_some());
            self.limit = self.map.len() * 2;
        }
        self.map.insert(key, weak);
    }
}

enum Ready {
    Unrequested,
    Pending(Deferred),
    Done,
}

/// What one realm's service-worker wrappers share.
pub(super) struct Space {
    /// `None` in a service worker's own realm, which cannot register or post.
    registry: Option<Rc<dyn ServiceWorkerRegistry>>,
    control: Control,
    task: Cell<Option<TaskId>>,
    container: RefCell<Option<WeakValue>>,
    workers: RefCell<WeakMap<u64>>,
    registrations: RefCell<WeakMap<u64>>,
    jobs: RefCell<HashMap<u64, Deferred>>,
    ready: RefCell<Ready>,
    /// `None` until the registry has been asked or told.
    controller: RefCell<Option<Option<WorkerRecord>>>,
}

impl Drop for Space {
    fn drop(&mut self) {
        self.control.close();
    }
}

fn illegal_constructor() -> OpError {
    OpError::type_error("Illegal constructor")
}

fn fire(ctx: &mut Ctx, target: &Value, kind: &str) {
    let event = ctx.new_instance(Event::trusted(kind));
    let _ = EventTarget::dispatch_trusted(ctx, target, &event);
}

fn unpin(pin: &RefCell<Option<Value>>) {
    let old = pin.borrow_mut().take();
    drop(old);
}

fn client_info(ctx: &mut Ctx) -> OpResult<ClientInfo> {
    let Some((url, origin)) = page_location(ctx) else {
        return Err(dom_error(
            ctx,
            "InvalidStateError",
            "The document has no location",
        ));
    };
    let secure = is_secure_context(&url);
    Ok(ClientInfo {
        url,
        origin,
        secure,
    })
}

fn no_registry(ctx: &mut Ctx) -> OpError {
    dom_error(
        ctx,
        "NotSupportedError",
        "Service workers are not available in this realm",
    )
}

impl Space {
    pub(super) fn new(registry: Option<Rc<dyn ServiceWorkerRegistry>>) -> Rc<Self> {
        Rc::new(Self {
            registry,
            control: Control::new(),
            task: Cell::new(None),
            container: RefCell::new(None),
            workers: RefCell::new(WeakMap::default()),
            registrations: RefCell::new(WeakMap::default()),
            jobs: RefCell::new(HashMap::new()),
            ready: RefCell::new(Ready::Unrequested),
            controller: RefCell::new(None),
        })
    }

    pub(super) fn control(&self) -> &Control {
        &self.control
    }

    /// Start listening for pushed events. The loop task never keeps the loop alive.
    fn listen(self: &Rc<Self>, ctx: &mut Ctx, container: &Value) -> OpResult<()> {
        *self.container.borrow_mut() = ctx.weak_value(container);
        let space = self.clone();
        let callback = ctx.new_native_fn(
            "",
            0,
            Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                space.drain(ctx);
                Ok(Value::Undefined)
            }),
        );
        let task = self.control.listen(ctx, callback)?;
        if let Some(tasks) = ctx.host_mut::<TaskRegistry>() {
            tasks.set_unref(task);
        }
        self.task.set(Some(task));
        Ok(())
    }

    /// Apply up to a batch of pushed events; a longer queue costs one more turn.
    pub(super) fn drain(self: &Rc<Self>, ctx: &mut Ctx) {
        for _ in 0..DRAIN_BATCH {
            match self.control.pop() {
                Pop::Message(event) => self.handle(ctx, event),
                Pop::Closed => {
                    if let Some(task) = self.task.take() {
                        self.control.unlisten(ctx, task);
                    }
                    return;
                }
                Pop::Empty => return,
            }
        }
        self.control.rewake();
    }

    /// Apply one pushed event. Events that belong to the host side of a service worker are
    /// ignored here.
    pub(super) fn handle(self: &Rc<Self>, ctx: &mut Ctx, event: WorkerEvent) {
        match event {
            WorkerEvent::Registration(record) => {
                let existing = self.registrations.borrow().get(&record.id);
                if let Some(wrapper) = existing {
                    let _ = self.apply(ctx, &wrapper, &record);
                }
                self.check_ready(ctx, &record);
            }
            WorkerEvent::StateChange { worker, state } => {
                let existing = self.workers.borrow().get(&worker);
                if let Some(wrapper) = existing {
                    let advanced = ctx
                        .with_instance::<bindings::ServiceWorker, _>(&wrapper, |w| {
                            if state > w.state.get() {
                                w.state.set(state);
                                if state == WorkerState::Redundant {
                                    unpin(&w.pin);
                                }
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if advanced {
                        fire(ctx, &wrapper, "statechange");
                    }
                }
            }
            WorkerEvent::UpdateFound { registration } => {
                let existing = self.registrations.borrow().get(&registration);
                if let Some(wrapper) = existing {
                    fire(ctx, &wrapper, "updatefound");
                }
            }
            WorkerEvent::ControllerChange(controller) => {
                *self.controller.borrow_mut() = Some(controller);
                let container = self.container.borrow().as_ref().and_then(WeakValue::upgrade);
                if let Some(container) = container {
                    fire(ctx, &container, "controllerchange");
                }
            }
            WorkerEvent::JobSettled { job, result } => {
                let deferred = self.jobs.borrow_mut().remove(&job);
                let Some(deferred) = deferred else {
                    return;
                };
                match result {
                    Ok(record) => match self.registration_wrapper(ctx, &record) {
                        Ok(wrapper) => {
                            deferred.resolve(ctx, wrapper);
                            self.check_ready(ctx, &record);
                        }
                        Err(error) => deferred.reject(ctx, error),
                    },
                    Err((name, message)) => {
                        let error = dom_error(ctx, &name, message);
                        deferred.reject(ctx, error);
                    }
                }
            }
            WorkerEvent::ServiceMessage { source, message } => {
                self.deliver_message(ctx, &source, message);
            }
            _ => {}
        }
    }

    fn deliver_message(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        source: &WorkerRecord,
        message: crate::clone_transfer::CloneMessage,
    ) {
        let container = self.container.borrow().as_ref().and_then(WeakValue::upgrade);
        let Some(container) = container else {
            return;
        };
        let Ok(worker) = self.worker_wrapper(ctx, source) else {
            return;
        };
        let origin = lumen_common::url::parse(&source.script_url, None)
            .map(|url| url.origin())
            .unwrap_or_default();
        let event = deserialize_message(ctx, message).and_then(|(data, ports)| match data {
            Ok(data) => MessageEvent::create(ctx, "message", data, &origin, worker, ports),
            Err(error) => {
                let error = error.to_value(ctx);
                MessageEvent::create(ctx, "messageerror", error, &origin, Value::Null, Vec::new())
            }
        });
        if let Ok(event) = event {
            let _ = EventTarget::dispatch_trusted(ctx, &container, &event);
        }
    }

    /// Resolve `ready` when `record` is the registration that controls this page and has an
    /// active worker.
    fn check_ready(self: &Rc<Self>, ctx: &mut Ctx, record: &RegistrationRecord) {
        if record.active.is_none() || !matches!(*self.ready.borrow(), Ready::Pending(_)) {
            return;
        }
        let Ok(client) = client_info(ctx) else {
            return;
        };
        if !client.url.starts_with(&record.scope) {
            return;
        }
        let Ready::Pending(deferred) = std::mem::replace(&mut *self.ready.borrow_mut(), Ready::Done)
        else {
            return;
        };
        match self.registration_wrapper(ctx, record) {
            Ok(wrapper) => deferred.resolve(ctx, wrapper),
            Err(error) => deferred.reject(ctx, error),
        }
    }

    /// The `ServiceWorker` of `record`: the cached wrapper when one is alive, else a new one in
    /// the record's state.
    pub(super) fn worker_wrapper(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        record: &WorkerRecord,
    ) -> OpResult<Value> {
        if let Some(wrapper) = self.workers.borrow().get(&record.id) {
            return Ok(wrapper);
        }
        let base = EventTarget::from_data(TargetData::new(None));
        base.data().observe_changes(worker_listeners_changed);
        let wrapper = ctx.new_instance(bindings::ServiceWorker {
            base,
            id: record.id,
            script_url: record.script_url.clone(),
            state: Cell::new(record.state),
            space: self.clone(),
            pin: RefCell::new(None),
        });
        ctx.set_native_identity_owner::<bindings::ServiceWorker>(&wrapper)?;
        if let Some(weak) = ctx.weak_value(&wrapper) {
            self.workers.borrow_mut().insert(record.id, weak);
        }
        Ok(wrapper)
    }

    /// The `ServiceWorkerRegistration` of `record`, updated to the record's workers.
    pub(super) fn registration_wrapper(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        record: &RegistrationRecord,
    ) -> OpResult<Value> {
        let existing = self.registrations.borrow().get(&record.id);
        if let Some(wrapper) = existing {
            self.apply(ctx, &wrapper, record)?;
            return Ok(wrapper);
        }
        let base = EventTarget::from_data(TargetData::new(None));
        base.data().observe_changes(registration_listeners_changed);
        let wrapper = ctx.new_instance(bindings::ServiceWorkerRegistration {
            base,
            id: record.id,
            scope: record.scope.clone(),
            space: self.clone(),
            installing: RefCell::new(Value::Null),
            waiting: RefCell::new(Value::Null),
            active: RefCell::new(Value::Null),
            update_via_cache: Cell::new(record.update_via_cache),
            pin: RefCell::new(None),
        });
        ctx.set_native_identity_owner::<bindings::ServiceWorkerRegistration>(&wrapper)?;
        self.apply(ctx, &wrapper, record)?;
        if let Some(weak) = ctx.weak_value(&wrapper) {
            self.registrations.borrow_mut().insert(record.id, weak);
        }
        Ok(wrapper)
    }

    fn apply(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        wrapper: &Value,
        record: &RegistrationRecord,
    ) -> OpResult<()> {
        let slot = |ctx: &mut Ctx, worker: &Option<WorkerRecord>| match worker {
            Some(worker) => self.worker_wrapper(ctx, worker),
            None => Ok(Value::Null),
        };
        let installing = slot(ctx, &record.installing)?;
        let waiting = slot(ctx, &record.waiting)?;
        let active = slot(ctx, &record.active)?;
        let _ = ctx.with_instance::<bindings::ServiceWorkerRegistration, _>(wrapper, |r| {
            *r.installing.borrow_mut() = installing;
            *r.waiting.borrow_mut() = waiting;
            *r.active.borrow_mut() = active;
            r.update_via_cache.set(record.update_via_cache);
        });
        Ok(())
    }

    fn registry(&self, ctx: &mut Ctx) -> OpResult<Rc<dyn ServiceWorkerRegistry>> {
        self.registry.clone().ok_or_else(|| no_registry(ctx))
    }

    /// Track a job whose result the registry will push.
    fn track(&self, ctx: &mut Ctx, job: u64) -> Promise<Value> {
        let deferred = Deferred::new(ctx);
        let promise = Promise::pending(&deferred);
        self.jobs.borrow_mut().insert(job, deferred);
        promise
    }

    fn controller_record(&self, ctx: &mut Ctx) -> OpResult<Option<WorkerRecord>> {
        if let Some(known) = &*self.controller.borrow() {
            return Ok(known.clone());
        }
        let registry = self.registry(ctx)?;
        let client = client_info(ctx)?;
        let record = registry.controller(&client);
        *self.controller.borrow_mut() = Some(record.clone());
        Ok(record)
    }
}

// ---- pins -------------------------------------------------------------------------------------

fn worker_listeners_changed(ctx: &mut Ctx, worker: &Value, data: &TargetData) {
    let _ = ctx.with_instance::<bindings::ServiceWorker, _>(worker, |w| {
        let live = w.state.get() != WorkerState::Redundant
            && (data.listener_count("statechange") > 0 || data.listener_count("error") > 0);
        if live {
            *w.pin.borrow_mut() = Some(worker.clone());
        } else {
            unpin(&w.pin);
        }
    });
}

fn registration_listeners_changed(ctx: &mut Ctx, registration: &Value, data: &TargetData) {
    let _ = ctx.with_instance::<bindings::ServiceWorkerRegistration, _>(registration, |r| {
        if data.listener_count("updatefound") > 0 {
            *r.pin.borrow_mut() = Some(registration.clone());
        } else {
            unpin(&r.pin);
        }
    });
}

// ---- options ----------------------------------------------------------------------------------

struct RegisterOptions {
    scope: Option<String>,
    module: bool,
    update_via_cache: UpdateViaCache,
}

fn enum_error(value: &str, kind: &str) -> OpError {
    OpError::type_error(format!(
        "Failed to execute 'register' on 'ServiceWorkerContainer': The provided value '{value}' is not a valid enum value of type {kind}."
    ))
}

/// `RegistrationOptions` in dictionary order: `scope`, `type`, `updateViaCache`.
fn read_register_options(ctx: &mut Ctx, options: &Value) -> OpResult<RegisterOptions> {
    let mut parsed = RegisterOptions {
        scope: None,
        module: false,
        update_via_cache: UpdateViaCache::Imports,
    };
    match options {
        Value::Undefined | Value::Null => return Ok(parsed),
        Value::Obj(_) => {}
        other => {
            return Err(crate::webidl::invalid_arg_type(
                ctx,
                "options",
                "of type object",
                other,
            ));
        }
    }
    let scope = ctx.member_get(options, "scope").map_err(OpError::thrown)?;
    if !matches!(scope, Value::Undefined) {
        parsed.scope = Some(usv_string(ctx, &scope).map_err(OpError::thrown)?);
    }
    let kind = ctx.member_get(options, "type").map_err(OpError::thrown)?;
    if !matches!(kind, Value::Undefined) {
        let text = ctx.coerce_string(&kind).map_err(OpError::thrown)?.to_string();
        parsed.module = match text.as_str() {
            "classic" => false,
            "module" => true,
            _ => return Err(enum_error(&text, "WorkerType")),
        };
    }
    let via = ctx
        .member_get(options, "updateViaCache")
        .map_err(OpError::thrown)?;
    if !matches!(via, Value::Undefined) {
        let text = ctx.coerce_string(&via).map_err(OpError::thrown)?.to_string();
        parsed.update_via_cache = UpdateViaCache::parse(&text)
            .ok_or_else(|| enum_error(&text, "ServiceWorkerUpdateViaCache"))?;
    }
    Ok(parsed)
}

fn resolve_url(input: &str, base: &str) -> OpResult<String> {
    lumen_common::url::parse(input, Some(base))
        .map(|url| url.href())
        .map_err(|_| OpError::type_error(format!("Failed to parse URL from {input}")))
}

fn register_job(
    ctx: &mut Ctx,
    space: &Rc<Space>,
    script_url: Option<Value>,
    options: Value,
) -> OpResult<Promise<Value>> {
    let Some(script_url) = script_url else {
        return Err(OpError::type_error(
            "Failed to execute 'register' on 'ServiceWorkerContainer': 1 argument required, but only 0 present.",
        ));
    };
    let script = usv_string(ctx, &script_url).map_err(OpError::thrown)?;
    let options = read_register_options(ctx, &options)?;
    let registry = space.registry(ctx)?;
    let client = client_info(ctx)?;
    let script_url = resolve_url(&script, &client.url)?;
    let scope = match &options.scope {
        Some(scope) => Some(resolve_url(scope, &client.url)?),
        None => None,
    };
    let job = registry
        .register(
            &client,
            RegisterRequest {
                script_url,
                scope,
                module: options.module,
                update_via_cache: options.update_via_cache,
            },
        )
        .map_err(OpError::from)?;
    Ok(space.track(ctx, job))
}

/// The registration among `records` whose scope is the longest prefix of `url`.
fn best_match<'a>(
    records: &'a [RegistrationRecord],
    url: &str,
) -> Option<&'a RegistrationRecord> {
    records
        .iter()
        .filter(|record| url.starts_with(&record.scope))
        .max_by_key(|record| record.scope.len())
}

fn get_registration(
    ctx: &mut Ctx,
    space: &Rc<Space>,
    client_url: Value,
) -> OpResult<Value> {
    let registry = space.registry(ctx)?;
    let client = client_info(ctx)?;
    let text = match client_url {
        Value::Undefined => String::new(),
        other => usv_string(ctx, &other).map_err(OpError::thrown)?,
    };
    let url = lumen_common::url::parse(&text, Some(&client.url))
        .map_err(|_| OpError::type_error(format!("Failed to parse URL from {text}")))?;
    if url.origin() != client.origin {
        return Err(dom_error(
            ctx,
            "SecurityError",
            "The clientURL must be same-origin with the document",
        ));
    }
    let href = url.href();
    let records = registry.registrations(&client);
    match best_match(&records, &href) {
        Some(record) => space.registration_wrapper(ctx, record),
        None => Ok(Value::Undefined),
    }
}

fn get_registrations(ctx: &mut Ctx, space: &Rc<Space>) -> OpResult<Value> {
    let registry = space.registry(ctx)?;
    let client = client_info(ctx)?;
    let records = registry.registrations(&client);
    let mut wrappers = Vec::with_capacity(records.len());
    for record in &records {
        wrappers.push(space.registration_wrapper(ctx, record)?);
    }
    Ok(ctx.make_array(wrappers))
}

fn ready(ctx: &mut Ctx, space: &Rc<Space>, slot: &str, this: &Value) -> OpResult<Value> {
    if let Some(promise) = ctx.native_private_value_slot(this, slot) {
        return Ok(promise);
    }
    let registry = space.registry(ctx)?;
    let client = client_info(ctx)?;
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    ctx.define_native_private_value_slot(this, slot, promise.clone())
        .map_err(OpError::thrown)?;
    let records = registry.registrations(&client);
    let active: Vec<RegistrationRecord> = records
        .into_iter()
        .filter(|record| record.active.is_some())
        .collect();
    match best_match(&active, &client.url) {
        Some(record) => {
            *space.ready.borrow_mut() = Ready::Done;
            let wrapper = space.registration_wrapper(ctx, record)?;
            deferred.resolve(ctx, wrapper);
        }
        None => *space.ready.borrow_mut() = Ready::Pending(deferred),
    }
    Ok(promise)
}

#[lumen_bind::module(name = "serviceWorkers")]
pub mod bindings {
    use super::*;

    #[class(name = "ServiceWorker", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct ServiceWorker {
        pub(super) base: EventTarget,
        pub(super) id: u64,
        pub(super) script_url: String,
        pub(super) state: Cell<WorkerState>,
        pub(super) space: Rc<Space>,
        pub(super) pin: RefCell<Option<Value>>,
    }

    #[class(name = "ServiceWorkerRegistration", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct ServiceWorkerRegistration {
        pub(super) base: EventTarget,
        pub(super) id: u64,
        pub(super) scope: String,
        pub(super) space: Rc<Space>,
        pub(super) installing: RefCell<Value>,
        pub(super) waiting: RefCell<Value>,
        pub(super) active: RefCell<Value>,
        pub(super) update_via_cache: Cell<UpdateViaCache>,
        pub(super) pin: RefCell<Option<Value>>,
    }

    #[methods]
    impl ServiceWorker {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter(name = "scriptURL")]
        fn script_url(&self) -> String {
            self.script_url.clone()
        }

        #[getter]
        fn state(&self) -> &'static str {
            self.state.get().as_str()
        }

        #[method(name = "postMessage")]
        fn post_message(
            &self,
            ctx: &mut Ctx,
            message: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            let registry = self.space.registry(ctx)?;
            if self.state.get() == WorkerState::Redundant {
                return Err(dom_error(
                    ctx,
                    "InvalidStateError",
                    "ServiceWorker is in the redundant state",
                ));
            }
            let client = client_info(ctx)?;
            let message = serialize_message(ctx, message, options)?;
            registry
                .post_to_worker(&client, self.id, message)
                .map_err(OpError::from)
        }

        #[getter]
        fn onstatechange(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "statechange")
        }

        #[setter]
        fn set_onstatechange(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "statechange", value)
        }

        #[getter]
        fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "error")
        }

        #[setter]
        fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "error", value)
        }
    }

    impl NativeIdentityOwner for ServiceWorker {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }

    #[methods]
    impl ServiceWorkerRegistration {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter]
        fn installing(&self) -> Value {
            self.installing.borrow().clone()
        }

        #[getter]
        fn waiting(&self) -> Value {
            self.waiting.borrow().clone()
        }

        #[getter]
        fn active(&self) -> Value {
            self.active.borrow().clone()
        }

        #[getter]
        fn scope(&self) -> String {
            self.scope.clone()
        }

        #[getter]
        fn update_via_cache(&self) -> &'static str {
            self.update_via_cache.get().as_str()
        }

        fn update(&self, ctx: &mut Ctx) -> Promise<Value> {
            let started = (|| -> OpResult<Promise<Value>> {
                let registry = self.space.registry(ctx)?;
                let client = client_info(ctx)?;
                let job = registry
                    .update(&client, self.id)
                    .map_err(OpError::from)?;
                Ok(self.space.track(ctx, job))
            })();
            started.unwrap_or_else(|error| Promise::rejected(error))
        }

        fn unregister(&self, ctx: &mut Ctx) -> Promise<bool> {
            let result = (|| -> OpResult<bool> {
                let registry = self.space.registry(ctx)?;
                let client = client_info(ctx)?;
                registry
                    .unregister(&client, self.id)
                    .map_err(OpError::from)
            })();
            Promise::ready(result)
        }

        #[getter]
        fn onupdatefound(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "updatefound")
        }

        #[setter]
        fn set_onupdatefound(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "updatefound", value)
        }
    }

    impl NativeIdentityOwner for ServiceWorkerRegistration {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
            visit(&self.installing.borrow());
            visit(&self.waiting.borrow());
            visit(&self.active.borrow());
        }
    }
}

#[lumen_bind::module(name = "serviceWorkerContainer")]
pub mod container_bindings {
    use super::*;

    #[class(name = "ServiceWorkerContainer", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct ServiceWorkerContainer {
        pub(super) base: EventTarget,
        pub(super) space: Rc<Space>,
        pub(super) ready_slot: String,
    }

    #[methods]
    impl ServiceWorkerContainer {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter]
        fn controller(&self, ctx: &mut Ctx) -> OpResult<Value> {
            match self.space.controller_record(ctx)? {
                Some(record) => self.space.worker_wrapper(ctx, &record),
                None => Ok(Value::Null),
            }
        }

        #[getter]
        fn ready(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            ready(ctx, &self.space, &self.ready_slot, &this.0)
        }

        #[method(hint(js(length = "1")))]
        fn register(
            &self,
            ctx: &mut Ctx,
            script_url: Passed<Value>,
            #[default(Value::Undefined)] options: Value,
        ) -> Promise<Value> {
            register_job(ctx, &self.space, script_url.0, options).unwrap_or_else(|error| Promise::rejected(error))
        }

        #[method(name = "getRegistration")]
        fn get_registration(
            &self,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] client_url: Value,
        ) -> Promise<Value> {
            Promise::ready(get_registration(ctx, &self.space, client_url))
        }

        #[method(name = "getRegistrations")]
        fn get_registrations(&self, ctx: &mut Ctx) -> Promise<Value> {
            Promise::ready(get_registrations(ctx, &self.space))
        }

        #[method(name = "startMessages")]
        fn start_messages(&self) {}

        #[getter]
        fn oncontrollerchange(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "controllerchange")
        }

        #[setter]
        fn set_oncontrollerchange(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "controllerchange", value)
        }

        #[getter]
        fn onmessage(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "message")
        }

        #[setter]
        fn set_onmessage(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "message", value)
        }

        #[getter]
        fn onmessageerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "messageerror")
        }

        #[setter]
        fn set_onmessageerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "messageerror", value)
        }

        #[getter]
        fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "error")
        }

        #[setter]
        fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "error", value)
        }
    }

    impl NativeIdentityOwner for ServiceWorkerContainer {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }
}

pub use bindings::{ServiceWorker, ServiceWorkerRegistration};
pub use container_bindings::ServiceWorkerContainer;

/// The container of this realm, created (with its subscription) on first use.
fn container(
    ctx: &mut Ctx,
    registry: &Rc<dyn ServiceWorkerRegistry>,
    navigator: &Value,
    slot: &str,
) -> OpResult<Value> {
    if let Some(container) = ctx.native_private_value_slot(navigator, slot) {
        return Ok(container);
    }
    let client = client_info(ctx)?;
    let space = Space::new(Some(registry.clone()));
    let ready_slot = ctx.allocate_native_private_slot_name();
    let container = ctx.new_instance(ServiceWorkerContainer {
        base: EventTarget::from_data(TargetData::new(None)),
        space: space.clone(),
        ready_slot,
    });
    ctx.set_native_identity_owner::<ServiceWorkerContainer>(&container)?;
    ctx.define_native_private_value_slot(navigator, slot, container.clone())
        .map_err(OpError::thrown)?;
    space.listen(ctx, &container)?;
    registry.subscribe(&client, space.control().clone());
    Ok(container)
}

/// Give this realm `navigator.serviceWorker` and the `ServiceWorker`,
/// `ServiceWorkerRegistration` and `ServiceWorkerContainer` interfaces, backed by `registry`.
///
/// Call it from the realm's `Navigator` setup, after `navigator` exists. The accessor is defined on
/// the `navigator` object itself, so realms that never call this do not have the property at all;
/// where it exists it reads `undefined` while the realm's location is not a secure context (the
/// location is read on every access, so a document URL assigned after the install counts).
/// Reading it creates the container (and the registry subscription) on first use; a script that
/// never reads it costs one closure.
pub fn install_service_workers(
    ctx: &mut Ctx,
    registry: Rc<dyn ServiceWorkerRegistry>,
) -> Result<(), Value> {
    lazy_globals::<bindings::Module>(ctx)?;
    lazy_globals::<container_bindings::Module>(ctx)?;
    let global = ctx.global_object();
    let navigator = ctx.member_get(&global, "navigator")?;
    if !matches!(navigator, Value::Obj(_)) {
        return Err(ctx.make_error("TypeError", "service workers need a navigator object"));
    }
    let slot = ctx.allocate_native_private_slot_name();
    let weak = ctx
        .weak_value(&navigator)
        .expect("navigator is an object");
    let getter = ctx.new_native_fn(
        "serviceWorker",
        0,
        Rc::new(move |ctx: &mut Ctx, this: Value, _: &[Value]| {
            let navigator = weak
                .upgrade()
                .filter(|navigator| same(navigator, &this))
                .ok_or_else(|| {
                    crate::webidl::invalid_this("Navigator").to_value(ctx)
                })?;
            if let Some((url, _)) = page_location(ctx) {
                if !is_secure_context(&url) {
                    return Ok(Value::Undefined);
                }
            }
            container(ctx, &registry, &navigator, &slot).map_err(|error| error.to_value(ctx))
        }),
    );
    let descriptor = ctx.plain_object(&[
        ("get", getter),
        ("enumerable", Value::Bool(true)),
        ("configurable", Value::Bool(true)),
    ]);
    ctx.define_property_value(&navigator, Value::str("serviceWorker"), &descriptor)
}
