//! A service worker's global scope: `ServiceWorkerGlobalScope`, `ExtendableEvent`,
//! `ExtendableMessageEvent`, `FetchEvent`, `Clients`, `Client` and `WindowClient`.
//!
//! [`super::install_scope`] with [`super::ScopeKind::Service`] makes the realm's global a
//! `ServiceWorkerGlobalScope`. The host that runs the worker pushes events on the scope's
//! [`Control`](super::Control) (`Lifecycle`, `Fetch`, `ClientMessage`, and the registration and
//! state events its own `self.registration` follows); outcomes go back through
//! [`ServiceScopeHost`]: `event_settled` once every `waitUntil` and `respondWith` promise of an
//! event settled, and `fetch_response` as soon as a fetch event's response is read.
//!
//! Client messages ride the control, not an implicit port, so each carries its `source`; the
//! `Receiver::Custom` hook of the native receivers has no per-message source to offer.

use super::control::WorkerEvent;
use super::page::dom_error;
use super::registry::{
    ClientKind, ClientRecord, FetchOutcome, FetchRequest, LifecycleKind, ServiceScopeHost,
};
use super::scope::{scope_state, ScopeState, WorkerGlobalScope};
use super::service::{Space, WeakMap};
use crate::events::{node_handler_get, node_handler_set, Event, EventInit, EventTarget};
use crate::messaging::{
    deserialize_message, frozen_array, is_port, member, serialize_message, Owned,
};
use crate::net::{self, fetch_bindings};
use crate::webidl::{invalid_arg_type, usv_string};
use lumen::embed::{Ctx, NativeIdentityOwner, OpError, OpResult, Promise, Value};
use lumen_bind::This;
use lumen_os::channel::Pop;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

/// How many pushed events one wake applies before yielding to the loop.
const DRAIN_BATCH: usize = 32;

fn illegal_constructor() -> OpError {
    OpError::type_error("Illegal constructor")
}

// ---- extending an event's lifetime --------------------------------------------------------------

type Done = Box<dyn FnOnce(&mut Ctx, Result<(), String>)>;

/// The extend-lifetime state of one `ExtendableEvent`: the promises passed to `waitUntil` (and,
/// for a fetch event, the response being read) and what to run when all of them have settled
/// after the dispatch.
pub(super) struct Extend {
    pending: Cell<usize>,
    dispatching: Cell<bool>,
    finished: Cell<bool>,
    error: RefCell<Option<String>>,
    done: RefCell<Option<Done>>,
}

impl Extend {
    fn new(done: Option<Done>) -> Rc<Self> {
        Rc::new(Self {
            pending: Cell::new(0),
            dispatching: Cell::new(false),
            finished: Cell::new(false),
            error: RefCell::new(None),
            done: RefCell::new(done),
        })
    }

    fn active(&self, event: &Event) -> bool {
        self.dispatching.get() || event.dispatching.get() || self.pending.get() > 0
    }

    /// Run the completion once the dispatch is over and nothing is pending.
    fn check(&self, ctx: &mut Ctx) {
        if self.dispatching.get() || self.pending.get() > 0 || self.finished.replace(true) {
            return;
        }
        let result = match self.error.borrow_mut().take() {
            Some(message) => Err(message),
            None => Ok(()),
        };
        let done = self.done.borrow_mut().take();
        if let Some(done) = done {
            done(ctx, result);
        }
    }

    fn fail(&self, message: String) {
        let mut error = self.error.borrow_mut();
        if error.is_none() {
            *error = Some(message);
        }
    }

    /// One more promise the event waits for.
    fn track(self: &Rc<Self>, ctx: &mut Ctx, promise: Value) {
        self.pending.set(self.pending.get() + 1);
        let settle = |ctx: &mut Ctx, extend: &Rc<Self>, fulfilled: bool| {
            let extend = extend.clone();
            ctx.new_native_fn(
                "",
                1,
                Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                    if !fulfilled {
                        let reason = args.first().cloned().unwrap_or(Value::Undefined);
                        extend.fail(describe(ctx, &reason));
                    }
                    extend.pending.set(extend.pending.get().saturating_sub(1));
                    extend.check(ctx);
                    Ok(Value::Undefined)
                }),
            )
        };
        let on_ok = settle(ctx, self, true);
        let on_err = settle(ctx, self, false);
        ctx.then_value(promise, on_ok, on_err);
    }

    fn release(self: &Rc<Self>, ctx: &mut Ctx) {
        self.pending.set(self.pending.get().saturating_sub(1));
        self.check(ctx);
    }
}

/// A rejection reason as the host sees it: its string form, capped.
fn describe(ctx: &mut Ctx, reason: &Value) -> String {
    let text = ctx
        .coerce_string(reason)
        .map(|text| text.to_string())
        .unwrap_or_else(|_| "unprintable rejection".to_owned());
    text.chars().take(1024).collect()
}

/// Dispatch a trusted event the host built, then let its lifetime end once what it extended
/// settles.
fn dispatch_extendable(ctx: &mut Ctx, global: &Value, event: &Value, extend: &Rc<Extend>) {
    extend.dispatching.set(true);
    let _ = EventTarget::dispatch_trusted(ctx, global, event);
    extend.dispatching.set(false);
}

// ---- Clients ----------------------------------------------------------------------------------

/// What the classes of one service worker realm share.
pub(super) struct ServiceState {
    host: Rc<dyn ServiceScopeHost>,
    pub(super) space: Rc<Space>,
    clients: RefCell<WeakMap<String>>,
    clients_object: OnceCell<Value>,
}

impl ServiceState {
    pub(super) fn new(host: Rc<dyn ServiceScopeHost>, space: Rc<Space>) -> Rc<Self> {
        Rc::new(Self {
            host,
            space,
            clients: RefCell::new(WeakMap::default()),
            clients_object: OnceCell::new(),
        })
    }

    fn client_wrapper(self: &Rc<Self>, ctx: &mut Ctx, record: &ClientRecord) -> OpResult<Value> {
        if let Some(wrapper) = self.clients.borrow().get(&record.id) {
            return Ok(wrapper);
        }
        let client = bindings::Client {
            id: record.id.clone(),
            url: record.url.clone(),
            kind: record.kind,
            frame_type: record.frame_type,
            state: self.clone(),
        };
        let wrapper = match record.kind {
            ClientKind::Window => ctx.new_instance(bindings::WindowClient { base: client }),
            _ => ctx.new_instance(client),
        };
        if let Some(weak) = ctx.weak_value(&wrapper) {
            self.clients.borrow_mut().insert(record.id.clone(), weak);
        }
        Ok(wrapper)
    }
}

fn client_origin(url: &str) -> String {
    lumen_common::url::parse(url, None)
        .map(|url| url.origin())
        .unwrap_or_default()
}

fn query_matches(kind: ClientKind, wanted: &str) -> bool {
    wanted == "all" || wanted == kind.as_str()
}

/// `ClientQueryOptions`: `includeUncontrolled` then `type`.
fn read_query(ctx: &mut Ctx, options: &Value) -> OpResult<(bool, String)> {
    match options {
        Value::Undefined | Value::Null => return Ok((false, "window".to_owned())),
        Value::Obj(_) => {}
        other => return Err(invalid_arg_type(ctx, "options", "of type object", other)),
    }
    let include = ctx
        .member_get(options, "includeUncontrolled")
        .map_err(OpError::thrown)?;
    let include = ctx.to_boolean(&include);
    let kind = ctx.member_get(options, "type").map_err(OpError::thrown)?;
    let kind = match kind {
        Value::Undefined => "window".to_owned(),
        value => {
            let text = ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string();
            if !matches!(text.as_str(), "window" | "worker" | "sharedworker" | "all") {
                return Err(OpError::type_error(format!(
                    "Failed to execute 'matchAll' on 'Clients': The provided value '{text}' is not a valid enum value of type ClientType."
                )));
            }
            text
        }
    };
    Ok((include, kind))
}

fn match_all(ctx: &mut Ctx, state: &Rc<ServiceState>, options: Value) -> OpResult<Value> {
    let (include, wanted) = read_query(ctx, &options)?;
    let records = state.host.match_all(include);
    let mut clients = Vec::with_capacity(records.len());
    for record in records.iter().filter(|r| query_matches(r.kind, &wanted)) {
        clients.push(state.client_wrapper(ctx, record)?);
    }
    Ok(ctx.make_array(clients))
}

fn get_client(ctx: &mut Ctx, state: &Rc<ServiceState>, id: &str) -> OpResult<Value> {
    match state.host.client(id) {
        Some(record) => state.client_wrapper(ctx, &record),
        None => Ok(Value::Undefined),
    }
}

// ---- events -----------------------------------------------------------------------------------

/// Run the host's completion of an event as an `Extend` completion.
fn settle_with(state: &Rc<ServiceState>, event: u64) -> Done {
    let host = state.host.clone();
    Box::new(move |_, result| host.event_settled(event, result))
}

fn lifecycle(
    ctx: &mut Ctx,
    global: &Value,
    state: &Rc<ServiceState>,
    event: u64,
    kind: LifecycleKind,
) {
    let extend = Extend::new(Some(settle_with(state, event)));
    let value = ctx.new_instance(bindings::ExtendableEvent {
        base: Event::trusted(kind.event_type()),
        ext: extend.clone(),
    });
    if ctx
        .set_native_identity_owner::<bindings::ExtendableEvent>(&value)
        .is_err()
    {
        state
            .host
            .event_settled(event, Err("could not create the event".into()));
        return;
    }
    dispatch_extendable(ctx, global, &value, &extend);
    extend.check(ctx);
}

fn client_message(
    ctx: &mut Ctx,
    global: &Value,
    state: &Rc<ServiceState>,
    event: u64,
    source: ClientRecord,
    message: crate::clone_transfer::CloneMessage,
) {
    let extend = Extend::new(Some(settle_with(state, event)));
    let built = (|| -> OpResult<Value> {
        let (data, ports) = deserialize_message(ctx, message)?;
        let (kind, data, ports) = match data {
            Ok(data) => ("message", data, ports),
            Err(_) => ("messageerror", Value::Null, Vec::new()),
        };
        let ports = frozen_array(ctx, ports)?;
        let client = state.client_wrapper(ctx, &source)?;
        let value = ctx.new_instance(bindings::ExtendableMessageEvent {
            base: bindings::ExtendableEvent {
                base: Event::trusted(kind),
                ext: extend.clone(),
            },
            data: RefCell::new(data),
            origin: client_origin(&source.url),
            last_event_id: String::new(),
            source: RefCell::new(client),
            ports: RefCell::new(ports),
        });
        ctx.set_native_identity_owner::<bindings::ExtendableMessageEvent>(&value)?;
        Ok(value)
    })();
    match built {
        Ok(value) => {
            dispatch_extendable(ctx, global, &value, &extend);
            extend.check(ctx);
        }
        Err(error) => {
            let thrown = error.to_value(ctx);
            let message = describe(ctx, &thrown);
            state.host.event_settled(event, Err(message));
        }
    }
}

fn fetch(ctx: &mut Ctx, global: &Value, state: &Rc<ServiceState>, request: FetchRequest) {
    let event = request.event;
    let extend = Extend::new(Some(settle_with(state, event)));
    let built = (|| -> OpResult<Value> {
        let value = net::request_from_parts(
            ctx,
            &request.method,
            &request.url,
            &request.headers,
            request.body,
            request.mode,
            request.credentials,
            request.redirect,
        )?;
        let init = EventInit {
            cancelable: true,
            trusted: true,
            ..EventInit::default()
        };
        let fetch_event = ctx.new_instance(bindings::FetchEvent {
            base: bindings::ExtendableEvent {
                base: Event::from_init("fetch", init),
                ext: extend.clone(),
            },
            request: value,
            client_id: request.client_id,
            resulting_client_id: String::new(),
            replaces_client_id: String::new(),
            response: RefCell::new(None),
        });
        ctx.set_native_identity_owner::<bindings::FetchEvent>(&fetch_event)?;
        Ok(fetch_event)
    })();
    let fetch_event = match built {
        Ok(fetch_event) => fetch_event,
        Err(error) => {
            let thrown = error.to_value(ctx);
            let message = describe(ctx, &thrown);
            state
                .host
                .fetch_response(event, FetchOutcome::Error(message.clone()));
            state.host.event_settled(event, Err(message));
            return;
        }
    };
    dispatch_extendable(ctx, global, &fetch_event, &extend);
    let response = ctx
        .with_instance::<bindings::FetchEvent, _>(&fetch_event, |e| e.response.borrow_mut().take())
        .ok()
        .flatten();
    match response {
        None => state.host.fetch_response(event, FetchOutcome::Fallback),
        Some(promise) => answer(ctx, state, &extend, event, promise),
    }
    extend.check(ctx);
}

/// Read what `respondWith` was given and report it; the event stays open until the body is read.
fn answer(
    ctx: &mut Ctx,
    state: &Rc<ServiceState>,
    extend: &Rc<Extend>,
    event: u64,
    promise: Value,
) {
    extend.pending.set(extend.pending.get() + 1);
    let reaction = |ctx: &mut Ctx, fulfilled: bool| {
        let state = state.clone();
        let extend = extend.clone();
        ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let settled = args.first().cloned().unwrap_or(Value::Undefined);
                if fulfilled {
                    respond(ctx, &state, &extend, event, settled);
                } else {
                    let message = describe(ctx, &settled);
                    state.host.fetch_response(event, FetchOutcome::Error(message));
                    extend.release(ctx);
                }
                Ok(Value::Undefined)
            }),
        )
    };
    let on_ok = reaction(ctx, true);
    let on_err = reaction(ctx, false);
    ctx.then_value(promise, on_ok, on_err);
}

fn respond(
    ctx: &mut Ctx,
    state: &Rc<ServiceState>,
    extend: &Rc<Extend>,
    event: u64,
    response: Value,
) {
    let head = net::served_response(ctx, &response).filter(|head| (100..=599).contains(&head.status));
    let Some(head) = head else {
        state.host.fetch_response(
            event,
            FetchOutcome::Error("TypeError: fetch event response must be a Response".into()),
        );
        extend.release(ctx);
        return;
    };
    let host = state.host.clone();
    let extend = extend.clone();
    net::read_served_body(
        ctx,
        &response,
        Box::new(move |ctx, body| {
            host.fetch_response(
                event,
                FetchOutcome::Response {
                    status: head.status,
                    status_text: head.status_text,
                    headers: head.headers,
                    body,
                },
            );
            extend.release(ctx);
        }),
    );
}

/// Apply the pushed events of a service worker realm.
pub(super) fn wake(ctx: &mut Ctx, global: &Value, scope: &Rc<ScopeState>) {
    let (Some(control), Some(state)) = (&scope.control, &scope.service) else {
        return;
    };
    for _ in 0..DRAIN_BATCH {
        match control.pop() {
            Pop::Message(WorkerEvent::Lifecycle { event, kind }) => {
                lifecycle(ctx, global, state, event, kind)
            }
            Pop::Message(WorkerEvent::Fetch(request)) => fetch(ctx, global, state, *request),
            Pop::Message(WorkerEvent::ClientMessage {
                event,
                source,
                message,
            }) => client_message(ctx, global, state, event, source, message),
            Pop::Message(other) => state.space.handle(ctx, other),
            Pop::Closed => {
                if let Some(task) = scope.task.take() {
                    control.unlisten(ctx, task);
                }
                return;
            }
            Pop::Empty => return,
        }
    }
    control.rewake();
}

fn state_of(ctx: &mut Ctx, this: &Value, interface: &str) -> OpResult<Rc<ServiceState>> {
    scope_state(ctx, this, interface)?
        .service
        .clone()
        .ok_or_else(|| crate::webidl::invalid_this(interface))
}

#[lumen_bind::module(name = "serviceWorkerScope")]
pub mod bindings {
    use super::*;

    #[class(name = "ExtendableEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct ExtendableEvent {
        pub(in crate::workers) base: Event,
        pub(in crate::workers) ext: Rc<Extend>,
    }

    #[class(name = "ExtendableMessageEvent", extends = ExtendableEvent, hint(js(webidl, invalid_this)))]
    pub struct ExtendableMessageEvent {
        pub(in crate::workers) base: ExtendableEvent,
        pub(in crate::workers) data: RefCell<Value>,
        pub(in crate::workers) origin: String,
        pub(in crate::workers) last_event_id: String,
        pub(in crate::workers) source: RefCell<Value>,
        pub(in crate::workers) ports: RefCell<Value>,
    }

    #[class(name = "FetchEvent", extends = ExtendableEvent, hint(js(webidl, invalid_this)))]
    pub struct FetchEvent {
        pub(in crate::workers) base: ExtendableEvent,
        pub(in crate::workers) request: Value,
        pub(in crate::workers) client_id: String,
        pub(in crate::workers) resulting_client_id: String,
        pub(in crate::workers) replaces_client_id: String,
        pub(in crate::workers) response: RefCell<Option<Value>>,
    }

    #[class(name = "Client", hint(js(webidl, invalid_this)))]
    pub struct Client {
        pub(in crate::workers) id: String,
        pub(in crate::workers) url: String,
        pub(in crate::workers) kind: ClientKind,
        pub(in crate::workers) frame_type: super::super::registry::FrameType,
        pub(in crate::workers) state: Rc<ServiceState>,
    }

    #[class(name = "WindowClient", extends = Client, hint(js(webidl, invalid_this)))]
    pub struct WindowClient {
        pub(in crate::workers) base: Client,
    }

    #[class(name = "Clients", hint(js(webidl, invalid_this)))]
    pub struct Clients {
        pub(in crate::workers) state: Rc<ServiceState>,
    }

    #[class(name = "ServiceWorkerGlobalScope", extends = WorkerGlobalScope, hint(js(webidl, invalid_this)))]
    pub struct ServiceWorkerGlobalScope {
        pub(in crate::workers) base: WorkerGlobalScope,
    }

    // ---- ExtendableEvent ---------------------------------------------------------------------

    #[methods]
    impl ExtendableEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Owned<Self>> {
            Ok(Owned(Self {
                base: Event::new(ctx, kind, options)?,
                ext: Extend::new(None),
            }))
        }

        #[method(name = "waitUntil")]
        fn wait_until(&self, ctx: &mut Ctx, promise: Value) -> OpResult<()> {
            if !self.ext.active(&self.base) {
                return Err(dom_error(
                    ctx,
                    "InvalidStateError",
                    "The event is no longer extendable",
                ));
            }
            self.ext.track(ctx, promise);
            Ok(())
        }
    }

    impl NativeIdentityOwner for ExtendableEvent {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.trace_native_values(visit);
        }
    }

    // ---- ExtendableMessageEvent --------------------------------------------------------------

    fn read_source(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
        match value {
            Value::Undefined | Value::Null => Ok(Value::Null),
            value
                if ctx.with_instance::<Client, _>(&value, |_| ()).is_ok()
                    || ctx.with_instance::<WindowClient, _>(&value, |_| ()).is_ok()
                    || ctx
                        .with_instance::<crate::workers::ServiceWorker, _>(&value, |_| ())
                        .is_ok()
                    || is_port(ctx, &value) =>
            {
                Ok(value)
            }
            value => Err(invalid_arg_type(
                ctx,
                "init.source",
                "an instance of Client, ServiceWorker or MessagePort",
                &value,
            )),
        }
    }

    #[methods]
    impl ExtendableMessageEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Owned<Self>> {
            let base = Event::new(ctx, kind, options.clone())?;
            let data = match member(ctx, &options, "data")? {
                Value::Undefined => Value::Null,
                data => data,
            };
            let origin = match member(ctx, &options, "origin")? {
                Value::Undefined => String::new(),
                value => usv_string(ctx, &value).map_err(OpError::thrown)?,
            };
            let last_event_id = match member(ctx, &options, "lastEventId")? {
                Value::Undefined => String::new(),
                value => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(),
            };
            let source = member(ctx, &options, "source")?;
            let source = read_source(ctx, source)?;
            let ports = match member(ctx, &options, "ports")? {
                Value::Undefined => Vec::new(),
                value @ Value::Obj(_) => ctx
                    .iterable_to_list(&value, usize::MAX)
                    .map_err(|_| OpError::type_error("ports is not iterable"))?,
                _ => return Err(OpError::type_error("ports is not iterable")),
            };
            let ports = frozen_array(ctx, ports)?;
            Ok(Owned(Self {
                base: ExtendableEvent {
                    base,
                    ext: Extend::new(None),
                },
                data: RefCell::new(data),
                origin,
                last_event_id,
                source: RefCell::new(source),
                ports: RefCell::new(ports),
            }))
        }

        #[getter]
        fn data(&self) -> Value {
            self.data.borrow().clone()
        }

        #[getter]
        fn origin(&self) -> String {
            self.origin.clone()
        }

        #[getter]
        fn last_event_id(&self) -> String {
            self.last_event_id.clone()
        }

        #[getter]
        fn source(&self) -> Value {
            self.source.borrow().clone()
        }

        #[getter]
        fn ports(&self) -> Value {
            self.ports.borrow().clone()
        }
    }

    impl NativeIdentityOwner for ExtendableMessageEvent {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.trace_native_values(visit);
            visit(&self.data.borrow());
            visit(&self.source.borrow());
            visit(&self.ports.borrow());
        }
    }

    // ---- FetchEvent --------------------------------------------------------------------------

    #[methods]
    impl FetchEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Owned<Self>> {
            let base = Event::new(ctx, kind, options.clone())?;
            let request = member(ctx, &options, "request")?;
            if ctx
                .with_instance::<fetch_bindings::Request, _>(&request, |_| ())
                .is_err()
            {
                return Err(invalid_arg_type(
                    ctx,
                    "init.request",
                    "an instance of Request",
                    &request,
                ));
            }
            let id = |ctx: &mut Ctx, key: &str| -> OpResult<String> {
                match member(ctx, &options, key)? {
                    Value::Undefined => Ok(String::new()),
                    value => Ok(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()),
                }
            };
            let client_id = id(ctx, "clientId")?;
            let replaces_client_id = id(ctx, "replacesClientId")?;
            let resulting_client_id = id(ctx, "resultingClientId")?;
            Ok(Owned(Self {
                base: ExtendableEvent {
                    base,
                    ext: Extend::new(None),
                },
                request,
                client_id,
                resulting_client_id,
                replaces_client_id,
                response: RefCell::new(None),
            }))
        }

        #[getter]
        fn request(&self) -> Value {
            self.request.clone()
        }

        #[getter]
        fn client_id(&self) -> String {
            self.client_id.clone()
        }

        #[getter]
        fn resulting_client_id(&self) -> String {
            self.resulting_client_id.clone()
        }

        #[getter]
        fn replaces_client_id(&self) -> String {
            self.replaces_client_id.clone()
        }

        #[method(name = "respondWith")]
        fn respond_with(&self, ctx: &mut Ctx, response: Value) -> OpResult<()> {
            if !self.base.base.dispatching.get() && !self.base.ext.dispatching.get() {
                return Err(dom_error(
                    ctx,
                    "InvalidStateError",
                    "The event handler is already finished",
                ));
            }
            if self.response.borrow().is_some() {
                return Err(dom_error(
                    ctx,
                    "InvalidStateError",
                    "respondWith was already called",
                ));
            }
            self.base.base.canceled.set(true);
            *self.response.borrow_mut() = Some(response);
            Ok(())
        }
    }

    impl NativeIdentityOwner for FetchEvent {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.trace_native_values(visit);
            visit(&self.request);
            if let Some(response) = &*self.response.borrow() {
                visit(response);
            }
        }
    }

    // ---- Client ------------------------------------------------------------------------------

    #[methods]
    impl Client {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter]
        fn id(&self) -> String {
            self.id.clone()
        }

        #[getter]
        fn url(&self) -> String {
            self.url.clone()
        }

        #[getter(name = "type")]
        fn kind(&self) -> &'static str {
            self.kind.as_str()
        }

        #[getter]
        fn frame_type(&self) -> &'static str {
            self.frame_type.as_str()
        }

        #[method(name = "postMessage")]
        fn post_message(
            &self,
            ctx: &mut Ctx,
            message: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            let message = serialize_message(ctx, message, options)?;
            self.state
                .host
                .post_to_client(&self.id, message)
                .map_err(OpError::from)
        }
    }

    #[methods]
    impl WindowClient {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter]
        fn visibility_state(&self) -> &'static str {
            "visible"
        }

        #[getter]
        fn focused(&self) -> bool {
            false
        }

        fn focus(this: This<Value>) -> Promise<Value> {
            Promise::resolved(this.0)
        }

        fn navigate(&self, ctx: &mut Ctx, #[default(Value::Undefined)] _url: Value) -> Promise<Value> {
            Promise::rejected(dom_error(
                ctx,
                "NotSupportedError",
                "Client navigation is unavailable",
            ))
        }
    }

    // ---- Clients -----------------------------------------------------------------------------

    #[methods]
    impl Clients {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        fn get(&self, ctx: &mut Ctx, id: Value) -> Promise<Value> {
            let result = ctx
                .coerce_string(&id)
                .map_err(OpError::thrown)
                .and_then(|id| get_client(ctx, &self.state, &id));
            Promise::ready(result)
        }

        #[method(name = "matchAll")]
        fn match_all(
            &self,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] options: Value,
        ) -> Promise<Value> {
            Promise::ready(match_all(ctx, &self.state, options))
        }

        #[method(name = "openWindow")]
        fn open_window(&self, ctx: &mut Ctx, #[default(Value::Undefined)] _url: Value) -> Promise<Value> {
            Promise::rejected(dom_error(
                ctx,
                "NotSupportedError",
                "Clients.openWindow is unavailable",
            ))
        }

        fn claim(&self, ctx: &mut Ctx) -> Promise<Value> {
            let claimed = self
                .state
                .host
                .claim()
                .map(|()| Value::Undefined)
                .map_err(OpError::from);
            let _ = ctx;
            Promise::ready(claimed)
        }
    }

    // ---- ServiceWorkerGlobalScope ------------------------------------------------------------

    #[methods]
    impl ServiceWorkerGlobalScope {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter]
        fn clients(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            let state = state_of(ctx, &this.0, "ServiceWorkerGlobalScope")?;
            if let Some(clients) = state.clients_object.get() {
                return Ok(clients.clone());
            }
            let clients = ctx.new_instance(Clients {
                state: state.clone(),
            });
            let _ = state.clients_object.set(clients.clone());
            Ok(clients)
        }

        #[getter]
        fn registration(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            let state = state_of(ctx, &this.0, "ServiceWorkerGlobalScope")?;
            let record = state.host.registration();
            state.space.registration_wrapper(ctx, &record)
        }

        #[getter(name = "serviceWorker")]
        fn service_worker(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            let state = state_of(ctx, &this.0, "ServiceWorkerGlobalScope")?;
            let record = state.host.worker();
            state.space.worker_wrapper(ctx, &record)
        }

        #[method(name = "skipWaiting")]
        fn skip_waiting(ctx: &mut Ctx, this: This<Value>) -> Promise<Value> {
            let result = state_of(ctx, &this.0, "ServiceWorkerGlobalScope").and_then(|state| {
                state
                    .host
                    .skip_waiting()
                    .map(|()| Value::Undefined)
                    .map_err(OpError::from)
            });
            Promise::ready(result)
        }

        #[getter]
        fn oninstall(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "install")
        }

        #[setter]
        fn set_oninstall(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "install", value)
        }

        #[getter]
        fn onactivate(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "activate")
        }

        #[setter]
        fn set_onactivate(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "activate", value)
        }

        #[getter]
        fn onfetch(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "fetch")
        }

        #[setter]
        fn set_onfetch(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "fetch", value)
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
    }
}

pub use bindings::{
    Client, Clients, ExtendableEvent, ExtendableMessageEvent, FetchEvent, ServiceWorkerGlobalScope,
    WindowClient,
};

/// `map` of the handlers a service worker global defines as own accessors, so a bare
/// `onfetch = ...` assignment reaches the realm's own handler.
pub(super) const HANDLERS: &[(&str, &str)] = &[
    ("oninstall", "install"),
    ("onactivate", "activate"),
    ("onfetch", "fetch"),
    ("onmessage", "message"),
    ("onmessageerror", "messageerror"),
];
