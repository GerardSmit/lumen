//! `Worker` and `SharedWorker`, the page side of the worker machinery.
//!
//! A `Worker` owns the outside end of the dedicated worker's implicit port pair. Its messages are
//! deserialized straight from the wire bytes and dispatched on the `Worker` by a native receiver
//! (`messaging::listen_native`), so `postMessage` honours transfer lists exactly like a
//! `MessagePort`. Everything else the backend reports (errors, the worker's exit) arrives on the
//! object's [`Control`].
//!
//! Lifetime. The control task keeps the event loop alive until the worker exits, as a running
//! worker always did. A `Worker` that has an `error`, `message` or `messageerror` listener and whose
//! worker is still running is pinned, so it survives without a script reference; any other `Worker`
//! is collectable, and collecting it drops its endpoint (the worker's posts are then dropped).
//! Collection never terminates the worker.

use super::backend::{backend, DedicatedSpec, SharedSpec, WorkerBackend};
use super::control::{Control, WorkerEvent};
use crate::events::{
    dom_exception, node_handler_get, node_handler_set, ErrorEvent, Event, EventInit, EventTarget,
    TargetData,
};
use crate::messaging::{listen_native, new_port, NativeReceiver, Receiver};
use crate::{ports, TaskId};
use lumen::embed::{Ctx, JsHost, NativeIdentityOwner, OpError, OpResult, Value, WeakValue};
use lumen_bind::{CtorRet, Host, This};
use lumen_common::cors::Credentials;
use lumen_common::worker::SharedWorkerKey;
use lumen_os::channel::Pop;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

/// A DOM exception as an error to throw.
pub(super) fn dom_error(ctx: &mut Ctx, name: &str, message: impl AsRef<str>) -> OpError {
    OpError::thrown(dom_exception(ctx, message.as_ref(), name))
}

fn unpin(pin: &RefCell<Option<Value>>) {
    let old = pin.borrow_mut().take();
    drop(old);
}

fn stop_control(ctx: &mut Ctx, control: &Control, task: &Cell<Option<TaskId>>) {
    if let Some(task) = task.take() {
        control.unlisten(ctx, task);
    }
}

fn listening(data: &TargetData, kinds: &[&str]) -> bool {
    kinds.iter().any(|kind| data.listener_count(kind) > 0)
}

// ---- options and URL resolution -------------------------------------------------------------------

struct Options {
    module: bool,
    name: String,
    credentials: Credentials,
}

fn enum_error(interface: &str, value: &str, kind: &str) -> OpError {
    OpError::type_error(format!(
        "Failed to construct '{interface}': The provided value '{value}' is not a valid enum value of type {kind}."
    ))
}

fn string_of(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    Ok(ctx.coerce_string(value).map_err(OpError::thrown)?.to_string())
}

/// `WorkerOptions` (`credentials`, `name`, `type`, read in dictionary order). `SharedWorker` also
/// takes a string, which is the name.
fn read_options(ctx: &mut Ctx, interface: &str, options: &Value) -> OpResult<Options> {
    let mut parsed = Options {
        module: false,
        name: String::new(),
        credentials: Credentials::SameOrigin,
    };
    match options {
        Value::Undefined | Value::Null => return Ok(parsed),
        Value::Str(name) if interface == "SharedWorker" => {
            parsed.name = name.to_string();
            return Ok(parsed);
        }
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
    let credentials = ctx.member_get(options, "credentials").map_err(OpError::thrown)?;
    if !matches!(credentials, Value::Undefined) {
        let text = string_of(ctx, &credentials)?;
        parsed.credentials = match text.as_str() {
            "omit" => Credentials::Omit,
            "same-origin" => Credentials::SameOrigin,
            "include" => Credentials::Include,
            _ => return Err(enum_error(interface, &text, "RequestCredentials")),
        };
    }
    let name = ctx.member_get(options, "name").map_err(OpError::thrown)?;
    if !matches!(name, Value::Undefined) {
        parsed.name = string_of(ctx, &name)?;
    }
    let kind = ctx.member_get(options, "type").map_err(OpError::thrown)?;
    if !matches!(kind, Value::Undefined) {
        let text = string_of(ctx, &kind)?;
        parsed.module = match text.as_str() {
            "classic" => false,
            "module" => true,
            _ => return Err(enum_error(interface, &text, "WorkerType")),
        };
    }
    Ok(parsed)
}

/// The page's `location.href` and `location.origin`, when it has a location.
pub(super) fn page_location(ctx: &mut Ctx) -> Option<(String, String)> {
    let global = ctx.global_object();
    let location = ctx.member_get(&global, "location").ok()?;
    if !matches!(location, Value::Obj(_)) {
        return None;
    }
    let Value::Str(href) = ctx.member_get(&location, "href").ok()? else {
        return None;
    };
    let origin = match ctx.member_get(&location, "origin") {
        Ok(Value::Str(origin)) => origin.to_string(),
        _ => String::new(),
    };
    Some((href.to_string(), origin))
}

/// The script URL of a dedicated worker and the owner origin the backend enforces. A page with a
/// location resolves against it and must stay same-origin; without one the string goes to the
/// backend as the path it names.
fn resolve_dedicated(ctx: &mut Ctx, script_url: &str) -> OpResult<(String, Option<String>)> {
    let Some((href, origin)) = page_location(ctx) else {
        let path = script_url.strip_prefix("file://").unwrap_or(script_url);
        return Ok((path.to_owned(), None));
    };
    let url = lumen_common::url::parse(script_url, Some(&href)).map_err(|_| {
        dom_error(
            ctx,
            "SyntaxError",
            format!("Failed to construct 'Worker': Invalid URL '{script_url}'"),
        )
    })?;
    if url.origin() != origin {
        return Err(dom_error(
            ctx,
            "SecurityError",
            "Worker script must be same-origin",
        ));
    }
    match url.scheme.as_str() {
        "http" | "https" | "file" => Ok((url.href(), Some(origin))),
        scheme => Err(dom_error(
            ctx,
            "NotSupportedError",
            format!("Unsupported worker URL scheme '{scheme}:'"),
        )),
    }
}

fn percent_decode_path(path: &str) -> Option<String> {
    String::from_utf8(lumen_common::codec::percent_decode_strict(path.as_bytes())?).ok()
}

/// A shared worker's identity, what it runs and whether that is fetched, from the constructor's
/// URL. The base is the page's location, or the realm's working directory without one.
fn resolve_shared(
    ctx: &mut Ctx,
    script_url: &str,
    options: &Options,
) -> OpResult<(SharedWorkerKey, String, bool)> {
    let location = page_location(ctx);
    let base = match &location {
        Some((href, _)) => href.clone(),
        None => match ctx.op_state().get::<crate::RealmProcess>() {
            Some(process) => {
                let mut base = format!("file://{}", process.cwd.to_string_lossy());
                if !base.ends_with('/') {
                    base.push('/');
                }
                base
            }
            None => "file:///".to_owned(),
        },
    };
    let parsed = lumen_common::url::parse(script_url, Some(&base)).map_err(|_| {
        dom_error(
            ctx,
            "SyntaxError",
            format!("Failed to construct 'SharedWorker': Invalid URL '{script_url}'"),
        )
    })?;
    if !matches!(parsed.scheme.as_str(), "file" | "http" | "https") {
        return Err(dom_error(
            ctx,
            "NotSupportedError",
            "shared worker scripts require file, http, or https URLs",
        ));
    }
    let script_origin = parsed.origin();
    let caller_origin = match &location {
        Some((_, origin)) => origin.clone(),
        None => script_origin.clone(),
    };
    if caller_origin != script_origin {
        return Err(dom_error(
            ctx,
            "SecurityError",
            "SharedWorker script must be same-origin",
        ));
    }
    let (url, entry, remote) = if parsed.scheme == "file" {
        let entry = percent_decode_path(&parsed.path).ok_or_else(|| {
            dom_error(ctx, "SyntaxError", "invalid escape in shared worker URL")
        })?;
        let canonical =
            std::fs::canonicalize(&entry).unwrap_or_else(|_| std::path::PathBuf::from(&entry));
        let mut url = format!("file://{}", canonical.to_string_lossy());
        if let Some(query) = parsed.query.as_deref() {
            url.push('?');
            url.push_str(query);
        }
        (url, canonical.to_string_lossy().into_owned(), false)
    } else {
        let mut parsed = parsed;
        parsed.fragment = None;
        let url = parsed.href();
        (url.clone(), url, true)
    };
    let key = SharedWorkerKey {
        url,
        origin: script_origin,
        is_module: options.module,
        credentials: options.credentials,
        name: options.name.clone(),
    };
    Ok((key, entry, remote))
}

// ---- Worker -----------------------------------------------------------------------------------

struct WorkerState {
    backend: Rc<dyn WorkerBackend>,
    id: u64,
    control: Control,
    task: Cell<Option<TaskId>>,
    pin: RefCell<Option<Value>>,
    terminated: Cell<bool>,
    exited: Cell<bool>,
}

const WORKER_EVENTS: &[&str] = &["message", "messageerror", "error"];

/// Pin the `Worker` while its worker runs and something listens to it, release it otherwise.
fn refresh_worker_pin(state: &WorkerState, worker: &Value, data: &TargetData) {
    let live = !state.exited.get() && !state.terminated.get();
    if live && listening(data, WORKER_EVENTS) {
        *state.pin.borrow_mut() = Some(worker.clone());
    } else {
        unpin(&state.pin);
    }
}

fn worker_listeners_changed(ctx: &mut Ctx, worker: &Value, data: &TargetData) {
    if let Ok(state) = ctx.with_instance::<bindings::Worker, _>(worker, |w| w.state.clone()) {
        refresh_worker_pin(&state, worker, data);
    }
}

fn flush_worker(ctx: &mut Ctx, worker: &Value) {
    let receiver = ctx
        .with_instance::<bindings::Worker, _>(worker, |w| w.receiver.get().cloned())
        .ok()
        .flatten();
    if let Some(receiver) = receiver {
        receiver.flush(ctx, worker);
    }
}

fn worker_wake(ctx: &mut Ctx, target: &WeakValue, state: &Rc<WorkerState>) {
    let event = match state.control.pop() {
        Pop::Message(event) => event,
        Pop::Closed => {
            stop_control(ctx, &state.control, &state.task);
            return;
        }
        Pop::Empty => return,
    };
    let worker = target.upgrade();
    match event {
        WorkerEvent::Error(message) => {
            if let Some(worker) = &worker {
                flush_worker(ctx, worker);
                if !state.terminated.get() {
                    let init = EventInit {
                        cancelable: true,
                        trusted: true,
                        ..EventInit::default()
                    };
                    if let Ok(event) =
                        ErrorEvent::create(ctx, "error", init, &message, "", 0, 0, Value::Null)
                    {
                        let _ = EventTarget::dispatch_trusted(ctx, worker, &event);
                    }
                }
            }
        }
        WorkerEvent::Exit(_) => {
            if let Some(worker) = &worker {
                flush_worker(ctx, worker);
            }
            state.exited.set(true);
            state.backend.exited(ctx, state.id);
            stop_control(ctx, &state.control, &state.task);
            unpin(&state.pin);
            return;
        }
        _ => {}
    }
    state.control.rewake();
}

/// A constructed `Worker` that still has to start receiving.
pub struct Started {
    worker: bindings::Worker,
    outside: u64,
}

impl CtorRet<JsHost, bindings::Worker> for Started {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let state = self.worker.state.clone();
        let instance = <JsHost as Host>::construct(cx, self.worker)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            start_worker(ctx, &instance, &state, self.outside).map_err(|error| {
                state.backend.terminate(ctx, state.id);
                error.to_value(ctx)
            })
        })?;
        Ok(instance)
    }
}

fn start_worker(
    ctx: &mut Ctx,
    instance: &Value,
    state: &Rc<WorkerState>,
    outside: u64,
) -> OpResult<()> {
    ctx.set_native_identity_owner::<bindings::Worker>(instance)?;
    let weak = ctx.weak_value(instance).expect("a Worker is an object");
    let receiver = listen_native(ctx, outside, weak.clone(), Receiver::Worker, None)?;
    let _ = ctx.with_instance::<bindings::Worker, _>(instance, |w| w.receiver.set(receiver));
    let callback = {
        let state = state.clone();
        ctx.new_native_fn(
            "",
            0,
            Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                worker_wake(ctx, &weak, &state);
                Ok(Value::Undefined)
            }),
        )
    };
    let task = state.control.listen(ctx, callback)?;
    state.task.set(Some(task));
    Ok(())
}

// ---- SharedWorker -----------------------------------------------------------------------------

struct SharedState {
    backend: Rc<dyn WorkerBackend>,
    id: u64,
    control: Control,
    task: Cell<Option<TaskId>>,
    pin: RefCell<Option<Value>>,
    /// The page closed its end of the port: nothing more is reported.
    detached: Cell<bool>,
    /// The worker's end is gone: `close` fired.
    closed: Cell<bool>,
}

const SHARED_EVENTS: &[&str] = &["error", "close"];

fn refresh_shared_pin(state: &SharedState, worker: &Value, data: &TargetData) {
    let live = !state.detached.get() && !state.closed.get();
    if live && listening(data, SHARED_EVENTS) {
        *state.pin.borrow_mut() = Some(worker.clone());
    } else {
        unpin(&state.pin);
    }
}

fn shared_listeners_changed(ctx: &mut Ctx, worker: &Value, data: &TargetData) {
    if let Ok(state) = ctx.with_instance::<bindings::SharedWorker, _>(worker, |w| w.state.clone())
    {
        refresh_shared_pin(&state, worker, data);
    }
}

/// Every queued event, in one turn: the worker's last words (`error`, then `close`) must reach the
/// page before the port's own close does.
fn shared_wake(ctx: &mut Ctx, target: &WeakValue, state: &Rc<SharedState>) {
    loop {
        let event = match state.control.pop() {
            Pop::Message(event) => event,
            Pop::Closed => {
                stop_control(ctx, &state.control, &state.task);
                unpin(&state.pin);
                return;
            }
            Pop::Empty => return,
        };
        if state.detached.get() || state.closed.get() {
            continue;
        }
        let Some(worker) = target.upgrade() else {
            if matches!(event, WorkerEvent::Close) {
                state.closed.set(true);
            }
            continue;
        };
        match event {
            WorkerEvent::Error(message) => {
                let init = EventInit {
                    cancelable: true,
                    trusted: true,
                    ..EventInit::default()
                };
                if let Ok(event) =
                    ErrorEvent::create(ctx, "error", init, &message, "", 0, 0, Value::Null)
                {
                    let _ = EventTarget::dispatch_trusted(ctx, &worker, &event);
                }
            }
            WorkerEvent::Close => {
                state.closed.set(true);
                unpin(&state.pin);
                let event = ctx.new_instance(Event::trusted("close"));
                let _ = EventTarget::dispatch_trusted(ctx, &worker, &event);
                stop_control(ctx, &state.control, &state.task);
                return;
            }
            _ => {}
        }
    }
}

/// A constructed `SharedWorker` that still has to start listening.
pub struct SharedStarted {
    worker: bindings::SharedWorker,
    port: Value,
    slot: String,
}

impl CtorRet<JsHost, bindings::SharedWorker> for SharedStarted {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let state = self.worker.state.clone();
        let instance = <JsHost as Host>::construct(cx, self.worker)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            start_shared(ctx, &instance, &state, &self.slot, self.port).map_err(|error| {
                state.backend.disconnect_shared(ctx, state.id);
                error.to_value(ctx)
            })
        })?;
        Ok(instance)
    }
}

fn start_shared(
    ctx: &mut Ctx,
    instance: &Value,
    state: &Rc<SharedState>,
    slot: &str,
    port: Value,
) -> OpResult<()> {
    ctx.define_native_private_value_slot(instance, slot, port)
        .map_err(OpError::thrown)?;
    ctx.set_native_identity_owner::<bindings::SharedWorker>(instance)?;
    let weak = ctx.weak_value(instance).expect("a SharedWorker is an object");
    let callback = {
        let state = state.clone();
        ctx.new_native_fn(
            "",
            0,
            Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                shared_wake(ctx, &weak, &state);
                Ok(Value::Undefined)
            }),
        )
    };
    let task = state.control.listen(ctx, callback)?;
    state.task.set(Some(task));
    Ok(())
}

/// The close hook of the page's end of a shared worker connection.
fn shared_disconnect(ctx: &mut Ctx, state: &Rc<SharedState>) {
    if state.detached.replace(true) {
        return;
    }
    unpin(&state.pin);
    stop_control(ctx, &state.control, &state.task);
    state.backend.disconnect_shared(ctx, state.id);
}

#[lumen_bind::module(name = "workers")]
pub mod bindings {
    use super::*;

    #[class(name = "Worker", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct Worker {
        pub(super) base: EventTarget,
        pub(super) state: Rc<WorkerState>,
        pub(super) receiver: OnceCell<NativeReceiver>,
    }

    #[class(name = "SharedWorker", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct SharedWorker {
        pub(super) base: EventTarget,
        pub(super) state: Rc<SharedState>,
        slot: String,
    }

    #[methods]
    impl Worker {
        #[constructor(coerce)]
        fn constructor(
            ctx: &mut Ctx,
            script_url: &str,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Started> {
            let options = read_options(ctx, "Worker", &options)?;
            let Some(backend) = backend(ctx) else {
                return Err(dom_error(
                    ctx,
                    "NotSupportedError",
                    "Worker is not supported in this environment",
                ));
            };
            let (url, owner_origin) = resolve_dedicated(ctx, script_url)?;
            let (outside, inside) = ports::new_pair();
            let control = Control::new();
            let spec = DedicatedSpec {
                url,
                module: options.module,
                name: options.name,
                credentials: options.credentials,
                owner_origin,
                inside,
                control: control.clone(),
            };
            let id = match backend.spawn_dedicated(ctx, spec) {
                Ok(id) => id,
                Err(error) => {
                    outside.close();
                    return Err(error.into());
                }
            };
            if !ports::available(ctx) {
                backend.terminate(ctx, id);
                backend.exited(ctx, id);
                outside.close();
                return Err(OpError::type_error(
                    "Workers require the message-ports extension",
                ));
            }
            let outside = ports::adopt(ctx, outside);
            let base = EventTarget::from_data(TargetData::new(None));
            base.data().observe_changes(worker_listeners_changed);
            Ok(Started {
                worker: Worker {
                    base,
                    state: Rc::new(WorkerState {
                        backend,
                        id,
                        control,
                        task: Cell::new(None),
                        pin: RefCell::new(None),
                        terminated: Cell::new(false),
                        exited: Cell::new(false),
                    }),
                    receiver: OnceCell::new(),
                },
                outside,
            })
        }

        #[method(name = "postMessage")]
        fn post_message(
            &self,
            ctx: &mut Ctx,
            message: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            if self.state.terminated.get() {
                return Ok(());
            }
            match self.receiver.get() {
                Some(receiver) => receiver.post(ctx, message, options),
                None => Ok(()),
            }
        }

        fn terminate(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
            if self.state.terminated.replace(true) {
                return Ok(());
            }
            unpin(&self.state.pin);
            self.state.backend.terminate(ctx, self.state.id);
            if let Some(receiver) = self.receiver.get().cloned() {
                receiver.close(ctx, &this.0);
            }
            Ok(())
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

    impl NativeIdentityOwner for Worker {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
            if let Some(receiver) = self.receiver.get() {
                receiver.trace(visit);
            }
        }
    }

    #[methods]
    impl SharedWorker {
        #[constructor(coerce)]
        fn constructor(
            ctx: &mut Ctx,
            script_url: &str,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<SharedStarted> {
            let options = read_options(ctx, "SharedWorker", &options)?;
            let Some(backend) = backend(ctx) else {
                return Err(dom_error(
                    ctx,
                    "NotSupportedError",
                    "SharedWorker is not supported in this environment",
                ));
            };
            let (key, entry, remote) = resolve_shared(ctx, script_url, &options)?;
            let (page_side, worker_side) = ports::new_pair();
            let control = Control::new();
            let spec = SharedSpec {
                key,
                entry,
                remote,
                page_side: page_side.clone(),
                worker_side,
                control: control.clone(),
            };
            let id = match backend.connect_shared(ctx, spec) {
                Ok(id) => id,
                Err(error) => {
                    page_side.close();
                    return Err(error.into());
                }
            };
            if !ports::available(ctx) {
                backend.disconnect_shared(ctx, id);
                page_side.close();
                return Err(OpError::type_error(
                    "Workers require the message-ports extension",
                ));
            }
            let state = Rc::new(SharedState {
                backend,
                id,
                control,
                task: Cell::new(None),
                pin: RefCell::new(None),
                detached: Cell::new(false),
                closed: Cell::new(false),
            });
            let hook = {
                let state = state.clone();
                ctx.new_native_fn(
                    "",
                    0,
                    Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                        shared_disconnect(ctx, &state);
                        Ok(Value::Undefined)
                    }),
                )
            };
            let port_id = ports::adopt(ctx, page_side);
            let port = new_port(ctx, port_id, Some(hook))?;
            let slot = ctx.allocate_native_private_slot_name();
            let base = EventTarget::from_data(TargetData::new(None));
            base.data().observe_changes(shared_listeners_changed);
            Ok(SharedStarted {
                worker: SharedWorker {
                    base,
                    state,
                    slot: slot.clone(),
                },
                port,
                slot,
            })
        }

        #[getter]
        fn port(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.slot)
                .unwrap_or(Value::Undefined)
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

    impl NativeIdentityOwner for SharedWorker {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }
}

pub use bindings::{SharedWorker, Worker};
