//! The worker global scopes: `WorkerGlobalScope`, `DedicatedWorkerGlobalScope`,
//! `SharedWorkerGlobalScope` and `WorkerLocation`.
//!
//! [`install_scope`] makes the realm's own global object an instance of the scope class
//! (`Ctx::attach_instance`, as `lumen-html-js` does for `Window`), so the prototype chain and every
//! `EventTarget` brand check are native. A dedicated scope receives its messages on the inside end
//! of the implicit port pair (a native receiver, so transfer lists work); a shared scope receives
//! `Connect` events on its [`Control`] and dispatches `connect` for each.
//!
//! Nothing is evaluated: the interface objects are lazy globals, the instance is built from the
//! host's values, and a worker that never touches `location` or `importScripts` never builds them.

use super::backend::{ScopeKind, WorkerScopeHost};
use super::control::{Control, WorkerEvent};
use crate::events::{
    node_handler_get, node_handler_set, report_exception, Event, EventTarget, TargetData,
};
use crate::messaging::{listen_native, new_port, MessageEvent, NativeReceiver, PromiseRejectionEvent, Receiver};
use crate::{lazy_globals, ports, TaskId};
use lumen::embed::{Ctx, HostRealmEvalError, OpError, OpResult, Value};
use lumen_bind::This;
use lumen_common::url::Url;
use lumen_os::channel::Pop;
use std::cell::{Cell, OnceCell};
use std::rc::Rc;

struct ScopeState {
    host: Rc<dyn WorkerScopeHost>,
    module: bool,
    name: String,
    url: Rc<Url>,
    location: OnceCell<Value>,
    receiver: OnceCell<NativeReceiver>,
    control: Option<Control>,
    task: Cell<Option<TaskId>>,
}

/// What a worker realm hands to [`install_scope`].
pub struct ScopeInstall {
    pub host: Rc<dyn WorkerScopeHost>,
    /// A dedicated worker's end of the implicit port pair.
    pub inside: Option<ports::PortTransfer>,
    /// A shared worker's control, on which clients connect.
    pub control: Option<Control>,
}

fn scope_state(ctx: &mut Ctx, this: &Value, interface: &str) -> OpResult<Rc<ScopeState>> {
    let receiver = match this {
        Value::Undefined | Value::Null => ctx.global_object(),
        other => other.clone(),
    };
    ctx.with_instance::<bindings::WorkerGlobalScope, _>(&receiver, |scope| scope.state.clone())
        .map_err(|_| crate::webidl::invalid_this(interface))
}

fn data_descriptor(ctx: &mut Ctx, value: Value) -> Value {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (key, field) in [
        ("value", value),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(true)),
        ("configurable", Value::Bool(true)),
    ] {
        let _ = ctx.member_set(&descriptor, key, field);
    }
    descriptor
}

/// An own accessor on the global: a bare identifier (`self`, `onmessage = ...`) then resolves
/// through the realm's own binding instead of creating an own data property.
fn define_accessor(
    ctx: &mut Ctx,
    global: &Value,
    name: &str,
    get: Value,
    set: Option<Value>,
) -> OpResult<()> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    let _ = ctx.member_set(&descriptor, "get", get);
    if let Some(set) = set {
        let _ = ctx.member_set(&descriptor, "set", set);
    }
    let _ = ctx.member_set(&descriptor, "enumerable", Value::Bool(true));
    let _ = ctx.member_set(&descriptor, "configurable", Value::Bool(true));
    ctx.define_property_value(global, Value::str(name), &descriptor)
        .map_err(OpError::thrown)
}

fn handler_accessor(ctx: &mut Ctx, global: &Value, name: &str, kind: &'static str) -> OpResult<()> {
    let get = ctx.new_native_fn(
        name,
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
            let global = ctx.global_object();
            node_handler_get(ctx, &global, kind).map_err(|error| error.to_value(ctx))
        }),
    );
    let set = ctx.new_native_fn(
        name,
        1,
        Rc::new(move |ctx: &mut Ctx, _this: Value, args: &[Value]| {
            let global = ctx.global_object();
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            node_handler_set(ctx, &global, kind, value).map_err(|error| error.to_value(ctx))?;
            Ok(Value::Undefined)
        }),
    );
    define_accessor(ctx, global, name, get, Some(set))
}

/// Make this realm's global a worker global scope, wired to `install.host`.
pub fn install_scope(ctx: &mut Ctx, install: ScopeInstall) -> OpResult<()> {
    let ScopeInstall {
        host,
        inside,
        control,
    } = install;
    let kind = host.kind();
    let url = lumen_common::url::parse(&host.location(), Some("file:///"))
        .or_else(|_| lumen_common::url::parse("file:///", None))
        .map_err(|_| OpError::type_error("invalid worker location"))?;
    let state = Rc::new(ScopeState {
        module: host.module(),
        name: host.name(),
        host: host.clone(),
        url: Rc::new(url),
        location: OnceCell::new(),
        receiver: OnceCell::new(),
        control,
        task: Cell::new(None),
    });
    let global = ctx.global_object();
    let base = bindings::WorkerGlobalScope {
        base: EventTarget::from_data(TargetData::new(None)),
        state: state.clone(),
    };
    lazy_globals::<bindings::Module>(ctx).map_err(OpError::thrown)?;
    match kind {
        ScopeKind::Dedicated => {
            lazy_globals::<dedicated::Module>(ctx).map_err(OpError::thrown)?;
            ctx.attach_instance(&global, dedicated::DedicatedWorkerGlobalScope { base })?;
        }
        ScopeKind::Shared => {
            lazy_globals::<shared::Module>(ctx).map_err(OpError::thrown)?;
            ctx.attach_instance(&global, shared::SharedWorkerGlobalScope { base })?;
        }
    }

    let get_self = ctx.new_native_fn(
        "self",
        0,
        Rc::new(|ctx: &mut Ctx, _this: Value, _: &[Value]| Ok(ctx.global_object())),
    );
    define_accessor(ctx, &global, "self", get_self, None)?;

    let report = {
        let host = host.clone();
        ctx.new_native_fn(
            "onerror",
            5,
            Rc::new(move |ctx: &mut Ctx, _this: Value, args: &[Value]| {
                let message = match args.first() {
                    Some(value) => ctx
                        .coerce_string(value)
                        .map(|text| text.to_string())
                        .unwrap_or_default(),
                    None => String::new(),
                };
                host.report_error(message);
                Ok(Value::Bool(true))
            }),
        )
    };
    let descriptor = data_descriptor(ctx, report);
    ctx.define_property_value(&global, Value::str("onerror"), &descriptor)
        .map_err(OpError::thrown)?;

    match kind {
        ScopeKind::Dedicated => {
            handler_accessor(ctx, &global, "onmessage", "message")?;
            handler_accessor(ctx, &global, "onmessageerror", "messageerror")?;
            let inside = inside
                .ok_or_else(|| OpError::type_error("a dedicated worker scope needs its port"))?;
            let id = ports::adopt(ctx, inside);
            let weak = ctx.weak_value(&global).expect("the global is an object");
            let receiver = listen_native(ctx, id, weak, Receiver::WorkerGlobal, None)?;
            receiver.set_ref(ctx, true);
            let _ = state.receiver.set(receiver);
        }
        ScopeKind::Shared => {
            handler_accessor(ctx, &global, "onconnect", "connect")?;
            let control = state
                .control
                .clone()
                .ok_or_else(|| OpError::type_error("a shared worker scope needs its control"))?;
            let weak = ctx.weak_value(&global).expect("the global is an object");
            let callback = {
                let state = state.clone();
                ctx.new_native_fn(
                    "",
                    0,
                    Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
                        if let Some(global) = weak.upgrade() {
                            scope_wake(ctx, &global, &state);
                        }
                        Ok(Value::Undefined)
                    }),
                )
            };
            let task = control.listen(ctx, callback)?;
            state.task.set(Some(task));
        }
    }
    Ok(())
}

fn scope_wake(ctx: &mut Ctx, global: &Value, state: &Rc<ScopeState>) {
    let Some(control) = &state.control else {
        return;
    };
    match control.pop() {
        Pop::Message(WorkerEvent::Connect(port)) => {
            let id = ports::adopt(ctx, port);
            let connected = new_port(ctx, id, None).and_then(|port| {
                MessageEvent::create(
                    ctx,
                    "connect",
                    Value::Null,
                    "",
                    port.clone(),
                    vec![port],
                )
            });
            match connected {
                Ok(event) => {
                    let _ = EventTarget::dispatch_trusted(ctx, global, &event);
                }
                Err(error) => {
                    let error = error.to_value(ctx);
                    report_exception(ctx, error);
                }
            }
            control.rewake();
        }
        Pop::Message(_) => control.rewake(),
        Pop::Closed => {
            if let Some(task) = state.task.take() {
                control.unlisten(ctx, task);
            }
        }
        Pop::Empty => {}
    }
}

/// Deliver one promise rejection notification to the worker global: `kind` is
/// `unhandledrejection` or `rejectionhandled`. The `on<kind>` handler runs first, then the
/// listeners; returns `false` when a handler cancelled the event.
pub fn dispatch_rejection(
    ctx: &mut Ctx,
    kind: &str,
    promise: Value,
    reason: Value,
) -> OpResult<bool> {
    let event = PromiseRejectionEvent::for_user_agent(ctx, kind, promise, reason)?;
    let global = ctx.global_object();
    let handler = ctx
        .member_get(&global, &format!("on{kind}"))
        .map_err(OpError::thrown)?;
    if handler.is_callable() {
        if let Err(error) = ctx.invoke(handler, global.clone(), std::slice::from_ref(&event)) {
            report_exception(ctx, error);
        }
    }
    EventTarget::dispatch_trusted(ctx, &global, &event)?;
    let prevented = ctx
        .with_instance::<Event, _>(&event, |event| event.default_prevented())
        .unwrap_or(false);
    Ok(!prevented)
}

fn execute_classic_script(ctx: &mut Ctx, source: &str, source_url: &str) -> OpResult<()> {
    let realm = ctx.current_host_realm();
    match ctx.eval_value_in_host_realm_named(&realm, source, false, Some(source_url)) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(exception)) => Err(OpError::thrown(exception)),
        Err(HostRealmEvalError::Parse(error)) => {
            let exception = if error.message == "dynamic code is unavailable in native execution" {
                ctx.make_error("EvalError", error.message)
            } else {
                ctx.make_error(
                    "SyntaxError",
                    format!("{}:{}: {}", source_url, error.line, error.message),
                )
            };
            Err(OpError::thrown(exception))
        }
        Err(HostRealmEvalError::Scope(_)) => Err(OpError::thrown(ctx.make_error(
            "Error",
            format!("could not enter worker script realm for {source_url}"),
        ))),
    }
}

fn import_scripts(ctx: &mut Ctx, state: &ScopeState, urls: &[String]) -> OpResult<()> {
    if state.module {
        return Err(OpError::type_error(
            "importScripts is unavailable in module workers",
        ));
    }
    let base = state.url.href();
    let mut resolved = Vec::with_capacity(urls.len());
    for url in urls {
        let parsed = lumen_common::url::parse(url, Some(&base)).map_err(|_| {
            super::page::dom_error(ctx, "SyntaxError", format!("Invalid URL '{url}'"))
        })?;
        resolved.push(parsed.href());
    }
    for url in resolved {
        let (final_url, source) = state
            .host
            .load_classic_script(ctx, &url)
            .map_err(OpError::from)?;
        let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
        execute_classic_script(ctx, source, &final_url)?;
    }
    Ok(())
}

fn location_of(ctx: &mut Ctx, state: &ScopeState) -> Value {
    if let Some(location) = state.location.get() {
        return location.clone();
    }
    let location = ctx.new_instance(bindings::WorkerLocation {
        url: state.url.clone(),
    });
    ctx.freeze_native_object(&location);
    let _ = state.location.set(location.clone());
    location
}

fn illegal_constructor() -> OpError {
    OpError::type_error("Illegal constructor")
}

#[lumen_bind::module(name = "workerScope")]
pub mod bindings {
    use super::*;

    #[class(name = "WorkerGlobalScope", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct WorkerGlobalScope {
        pub(super) base: EventTarget,
        pub(super) state: Rc<ScopeState>,
    }

    #[class(name = "WorkerLocation", hint(js(webidl, invalid_this)))]
    pub struct WorkerLocation {
        pub(super) url: Rc<Url>,
    }

    #[methods]
    impl WorkerGlobalScope {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        #[getter(name = "self")]
        fn self_scope(&self, ctx: &mut Ctx) -> Value {
            ctx.global_object()
        }

        #[getter]
        fn location(&self, ctx: &mut Ctx) -> Value {
            location_of(ctx, &self.state)
        }

        #[getter(name = "type")]
        fn kind(&self) -> &'static str {
            if self.state.module {
                "module"
            } else {
                "classic"
            }
        }

        fn close(ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
            scope_state(ctx, &this.0, "WorkerGlobalScope")?.host.close();
            Ok(())
        }

        #[method(name = "importScripts", coerce)]
        fn import_scripts(
            ctx: &mut Ctx,
            this: This<Value>,
            #[varargs] urls: Vec<String>,
        ) -> OpResult<()> {
            let state = scope_state(ctx, &this.0, "WorkerGlobalScope")?;
            import_scripts(ctx, &state, &urls)
        }
    }

    #[methods]
    impl WorkerLocation {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(illegal_constructor())
        }

        fn to_string(&self) -> String {
            self.url.href()
        }

        #[getter]
        fn href(&self) -> String {
            self.url.href()
        }

        #[getter]
        fn origin(&self) -> String {
            self.url.origin()
        }

        #[getter]
        fn protocol(&self) -> String {
            format!("{}:", self.url.scheme)
        }

        #[getter]
        fn host(&self) -> String {
            match &self.url.host {
                None => String::new(),
                Some(host) => match self.url.port {
                    Some(port) => format!("{host}:{port}"),
                    None => host.clone(),
                },
            }
        }

        #[getter]
        fn hostname(&self) -> String {
            self.url.hostname().to_owned()
        }

        #[getter]
        fn port(&self) -> String {
            self.url
                .port
                .map(|port| port.to_string())
                .unwrap_or_default()
        }

        #[getter]
        fn pathname(&self) -> String {
            self.url.path.clone()
        }

        #[getter]
        fn search(&self) -> String {
            match self.url.query.as_deref() {
                Some(query) if !query.is_empty() => format!("?{query}"),
                _ => String::new(),
            }
        }

        #[getter]
        fn hash(&self) -> String {
            match self.url.fragment.as_deref() {
                Some(fragment) if !fragment.is_empty() => format!("#{fragment}"),
                _ => String::new(),
            }
        }
    }
}

/// `DedicatedWorkerGlobalScope`, exposed in dedicated worker realms only.
pub mod dedicated {
    use super::*;

    #[lumen_bind::module(name = "dedicatedWorkerScope")]
    pub mod bindings {
        use super::super::bindings::WorkerGlobalScope;
        use super::*;

        #[class(name = "DedicatedWorkerGlobalScope", extends = WorkerGlobalScope, hint(js(webidl, invalid_this)))]
        pub struct DedicatedWorkerGlobalScope {
            pub(in super::super) base: WorkerGlobalScope,
        }

        #[methods]
        impl DedicatedWorkerGlobalScope {
            #[constructor]
            fn constructor() -> OpResult<Self> {
                Err(illegal_constructor())
            }

            #[getter]
            fn name(&self) -> String {
                self.base.state.name.clone()
            }

            #[method(name = "postMessage")]
            fn post_message(
                ctx: &mut Ctx,
                this: This<Value>,
                message: Value,
                #[default(Value::Undefined)] options: Value,
            ) -> OpResult<()> {
                let state = scope_state(ctx, &this.0, "DedicatedWorkerGlobalScope")?;
                match state.receiver.get().cloned() {
                    Some(receiver) => receiver.post(ctx, message, options),
                    None => Ok(()),
                }
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
            fn set_onmessageerror(
                ctx: &mut Ctx,
                this: This<Value>,
                value: Value,
            ) -> OpResult<()> {
                node_handler_set(ctx, &this.0, "messageerror", value)
            }
        }
    }

    pub use self::bindings::{DedicatedWorkerGlobalScope, Module};
}

/// `SharedWorkerGlobalScope`, exposed in shared worker realms only.
pub mod shared {
    use super::*;

    #[lumen_bind::module(name = "sharedWorkerScope")]
    pub mod bindings {
        use super::super::bindings::WorkerGlobalScope;
        use super::*;

        #[class(name = "SharedWorkerGlobalScope", extends = WorkerGlobalScope, hint(js(webidl, invalid_this)))]
        pub struct SharedWorkerGlobalScope {
            pub(in super::super) base: WorkerGlobalScope,
        }

        #[methods]
        impl SharedWorkerGlobalScope {
            #[constructor]
            fn constructor() -> OpResult<Self> {
                Err(illegal_constructor())
            }

            #[getter]
            fn name(&self) -> String {
                self.base.state.name.clone()
            }

            /// `self.name = value` makes the name an own data property of the global; deleting it
            /// restores this accessor.
            #[setter]
            fn set_name(&self, ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                let descriptor = data_descriptor(ctx, value);
                ctx.define_property_value(&this.0, Value::str("name"), &descriptor)
                    .map_err(OpError::thrown)
            }

            #[getter]
            fn onconnect(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                node_handler_get(ctx, &this.0, "connect")
            }

            #[setter]
            fn set_onconnect(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                node_handler_set(ctx, &this.0, "connect", value)
            }
        }
    }

    pub use self::bindings::{Module, SharedWorkerGlobalScope};
}

pub use bindings::{WorkerGlobalScope, WorkerLocation};
