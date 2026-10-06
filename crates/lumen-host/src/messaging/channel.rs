//! `MessagePort`, `MessageChannel` and `BroadcastChannel` over the endpoints of [`crate::ports`].
//!
//! A web port is an `EventTarget` that owns one endpoint handle (`Link`). Messages cross the
//! channel as structured-clone wire bytes: `postMessage` serializes synchronously through
//! [`crate::structured_clone`], the receiving realm wakes a loop task, and each message is
//! deserialized there and dispatched as its own task.
//!
//! Lifetime. The loop task holds its port weakly. A port that has started, has a `message`
//! listener and can still receive is pinned by its handle, so it survives without a script
//! reference; every other port is collectable, and a collected port reports itself to the
//! realm's reaper, which releases the handle and lets the peer drop its own pin.

use crate::events::{
    node_handler_get, node_handler_set, EventTarget, TargetData,
};
use crate::messaging::{MessageEvent, Slotted};
use crate::ports::{self, DeadPorts, Posted, RawPolled};
use crate::{clone_transfer, structured_clone};
use lumen::embed::{Ctx, NativeIdentityOwner, OpError, OpResult, Value, WeakValue};
use lumen_bind::NativeError;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// One realm-side endpoint of a port or broadcast channel.
pub(crate) struct Link {
    id: Cell<Option<u64>>,
    started: Cell<bool>,
    /// Called once when the endpoint closes (either side): the worker glue's disconnect hook.
    on_close: RefCell<Option<Value>>,
    dead: Option<Rc<DeadPorts>>,
}

impl Link {
    fn new(ctx: &mut Ctx, id: u64, on_close: Option<Value>) -> Rc<Self> {
        ports::reap(ctx);
        Rc::new(Self {
            id: Cell::new(Some(id)),
            started: Cell::new(false),
            on_close: RefCell::new(on_close),
            dead: Some(ports::dead_ports(ctx)),
        })
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let (Some(id), Some(dead)) = (self.id.get(), &self.dead) {
            dead.push(id);
        }
    }
}

fn require_ports(ctx: &mut Ctx) -> OpResult<()> {
    if ports::available(ctx) {
        Ok(())
    } else {
        Err(OpError::type_error("Message channels require the message-ports extension"))
    }
}

fn link_of(ctx: &mut Ctx, value: &Value) -> Option<Rc<Link>> {
    if let Ok(link) = ctx.with_instance::<bindings::MessagePort, _>(value, |port| port.link.clone())
    {
        return Some(link);
    }
    ctx.with_instance::<bindings::BroadcastChannel, _>(value, |channel| channel.link.clone())
        .ok()
}

fn new_target() -> EventTarget {
    let base = EventTarget::from_data(TargetData::new(None));
    base.data().observe_changes(listeners_changed);
    base
}

fn listeners_changed(ctx: &mut Ctx, receiver: &Value, _: &TargetData) {
    if let Some(link) = link_of(ctx, receiver) {
        refresh_pin(ctx, receiver, &link);
    }
}

/// A web `MessagePort` around the adopted endpoint `id`.
pub(crate) fn new_port(ctx: &mut Ctx, id: u64, on_close: Option<Value>) -> OpResult<Value> {
    let link = Link::new(ctx, id, on_close);
    let port = ctx.new_instance(bindings::MessagePort {
        base: new_target(),
        link,
    });
    ctx.set_native_identity_owner::<bindings::MessagePort>(&port)?;
    Ok(port)
}

// ---- lifetime and delivery ---------------------------------------------------------------------

fn has_message_listener(ctx: &mut Ctx, receiver: &Value) -> bool {
    EventTarget::of_receiver(ctx, receiver)
        .is_ok_and(|(data, _)| data.listener_count("message") > 0)
}

/// Pin the port while a message can still reach a listener, release it otherwise.
fn refresh_pin(ctx: &mut Ctx, receiver: &Value, link: &Link) {
    let Some(id) = link.id.get() else {
        return;
    };
    let keep = link.started.get()
        && has_message_listener(ctx, receiver)
        && ports::can_receive(ctx, id);
    ports::set_pin(ctx, id, keep.then(|| receiver.clone()));
}

/// Which event a native receiver dispatches for each message, and what the peer's close does.
#[derive(Clone, Copy)]
pub enum Receiver {
    /// A `MessageEvent` on a `MessagePort`; the peer's close fires `close` on it.
    Port,
    /// A `MessageEvent` on a `Worker` (the page end of a dedicated worker's implicit port).
    Worker,
    /// A `MessageEvent` on a worker global scope.
    WorkerGlobal,
    /// An event the host builds from the message's `data` and `ports` (a service worker's
    /// `ExtendableMessageEvent`).
    Custom(fn(&mut Ctx, Value, Vec<Value>) -> OpResult<Value>),
}

impl Receiver {
    fn message_event(self, ctx: &mut Ctx, data: Value, ports: Vec<Value>) -> OpResult<Value> {
        match self {
            Receiver::Custom(build) => build(ctx, data, ports),
            _ => MessageEvent::create(ctx, "message", data, "", Value::Null, ports),
        }
    }

    fn fires_close(self) -> bool {
        matches!(self, Receiver::Port)
    }
}

/// Start receiving: register the loop task that drains the endpoint. The task holds the target
/// and the link weakly: whoever owns them decides how long delivery lasts.
fn register_listener(
    ctx: &mut Ctx,
    id: u64,
    target: WeakValue,
    link: Weak<Link>,
    kind: Receiver,
) -> OpResult<()> {
    let callback = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, _: &[Value]| {
            if let (Some(target), Some(link)) = (target.upgrade(), link.upgrade()) {
                on_wake(ctx, &target, &link, kind);
            }
            Ok(Value::Undefined)
        }),
    );
    ports::listen(ctx, id, callback)
}

fn start_link(ctx: &mut Ctx, receiver: &Value, link: &Rc<Link>) -> OpResult<()> {
    let Some(id) = link.id.get() else {
        return Ok(());
    };
    if link.started.get() {
        return Ok(());
    }
    let weak = ctx.weak_value(receiver).expect("ports are objects");
    register_listener(ctx, id, weak, Rc::downgrade(link), Receiver::Port)?;
    link.started.set(true);
    refresh_pin(ctx, receiver, link);
    Ok(())
}

fn on_wake(ctx: &mut Ctx, receiver: &Value, link: &Rc<Link>, kind: Receiver) {
    let Some(id) = link.id.get() else {
        return;
    };
    ports::reap(ctx);
    match ports::poll_raw(ctx, id) {
        Err(_) => {
            finish(ctx, receiver, link, kind, false);
            return;
        }
        Ok(RawPolled::Closed) => {
            finish(ctx, receiver, link, kind, true);
            return;
        }
        Ok(RawPolled::Empty) => {}
        Ok(RawPolled::Message(bytes)) => {
            ports::rewake(ctx, id);
            deliver(ctx, receiver, bytes, kind);
        }
    }
    if matches!(kind, Receiver::Port) {
        refresh_pin(ctx, receiver, link);
    }
}

fn deliver(ctx: &mut Ctx, receiver: &Value, bytes: Vec<u8>, kind: Receiver) {
    let event = (|| -> OpResult<Value> {
        let bridge = web_bridge(ctx)?;
        ports::begin_received(ctx);
        let data = structured_clone::deserialize(ctx, &bytes, &bridge);
        let received = ports::end_received(ctx);
        match data {
            Ok(data) => kind.message_event(ctx, data, received),
            Err(error) => {
                let error = error.to_value(ctx);
                MessageEvent::create(ctx, "messageerror", error, "", Value::Null, Vec::new())
            }
        }
    })();
    if let Ok(event) = event {
        let _ = EventTarget::dispatch_trusted(ctx, receiver, &event);
    }
}

/// The endpoint is gone (the peer closed, or a transfer detached it): release the handle, run the
/// close hook and, for the peer's close, fire `close` on a port.
fn finish(ctx: &mut Ctx, receiver: &Value, link: &Rc<Link>, kind: Receiver, fire: bool) {
    let Some(id) = link.id.take() else {
        return;
    };
    ports::set_pin(ctx, id, None);
    ports::detach(ctx, id);
    let hook = link.on_close.borrow_mut().take();
    if let Some(hook) = hook {
        let _ = ctx.invoke(hook, Value::Undefined, &[]);
    }
    if fire && kind.fires_close() {
        let event = ctx.new_instance(crate::events::Event::trusted("close"));
        let _ = EventTarget::dispatch_trusted(ctx, receiver, &event);
    }
}

fn close_link(ctx: &mut Ctx, receiver: &Value, link: &Rc<Link>, kind: Receiver) {
    let Some(id) = link.id.get() else {
        return;
    };
    ports::close(ctx, id);
    finish(ctx, receiver, link, kind, false);
}

/// The receiving end of an endpoint whose messages a native class (a `Worker`, a worker global)
/// dispatches itself, instead of a `MessagePort` wrapping the endpoint. The owner keeps this
/// value for as long as it wants delivery: dropping it releases the endpoint through the realm's
/// reaper. Messages are deserialized straight from the wire bytes (no script buffer in between)
/// and each is its own loop task.
pub struct NativeReceiver {
    link: Rc<Link>,
    kind: Receiver,
}

impl NativeReceiver {
    /// Whether the endpoint is still attached (not closed locally or by the peer, not detached).
    pub fn is_open(&self) -> bool {
        self.link.id.get().is_some()
    }

    /// `postMessage(message, transfer)` on the endpoint, with the same transfer-list handling,
    /// limits and errors as a `MessagePort`. A closed endpoint drops the message.
    pub fn post(&self, ctx: &mut Ctx, message: Value, transfer: Value) -> OpResult<()> {
        if !self.is_open() {
            return Ok(());
        }
        post_link(ctx, &self.link, message, transfer)
    }

    /// Close the channel for both sides; `target` is the object events were dispatched on.
    pub fn close(&self, ctx: &mut Ctx, target: &Value) {
        close_link(ctx, target, &self.link, self.kind);
    }

    /// Keep `target` alive (or stop doing so) while the peer can still send.
    pub fn pin(&self, ctx: &mut Ctx, target: Option<Value>) {
        if let Some(id) = self.link.id.get() {
            ports::set_pin(ctx, id, target);
        }
    }

    /// Report the values the receiver holds to the collector.
    pub fn trace(&self, visit: &mut dyn FnMut(&Value)) {
        if let Some(hook) = &*self.link.on_close.borrow() {
            visit(hook);
        }
    }
}

/// Deliver the messages of the adopted endpoint `id` as events on `target`. Receiving starts at
/// once. `on_close` runs when the endpoint closes from either side.
pub fn listen_native(
    ctx: &mut Ctx,
    id: u64,
    target: WeakValue,
    kind: Receiver,
    on_close: Option<Value>,
) -> OpResult<NativeReceiver> {
    require_ports(ctx)?;
    let link = Link::new(ctx, id, on_close);
    register_listener(ctx, id, target, Rc::downgrade(&link), kind)?;
    link.started.set(true);
    Ok(NativeReceiver { link, kind })
}

// ---- posting -----------------------------------------------------------------------------------

/// The transfer list of `postMessage(message, transfer | options)` as an array.
fn transfer_list(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    match &value {
        Value::Undefined | Value::Null => return Ok(ctx.make_array(Vec::new())),
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
    if ctx.is_array_value(&value).map_err(OpError::thrown)? {
        return Ok(value);
    }
    let iterator = ctx.well_known_symbol("iterator").expect("Symbol.iterator");
    let method = ctx
        .reflect_get(&value, &iterator, &value)
        .map_err(OpError::thrown)?;
    let sequence = if method.is_callable() {
        value
    } else {
        match ctx.member_get(&value, "transfer").map_err(OpError::thrown)? {
            Value::Undefined => return Ok(ctx.make_array(Vec::new())),
            transfer => transfer,
        }
    };
    let items = ctx.iterable_to_list(&sequence, usize::MAX)?;
    Ok(ctx.make_array(items))
}

fn post_link(
    ctx: &mut Ctx,
    link: &Rc<Link>,
    message: Value,
    transfer: Value,
) -> OpResult<()> {
    let list = transfer_list(ctx, transfer)?;
    let list = structured_clone::array_items(ctx, &list)?;
    if link.id.get().is_some_and(|id| ports::is_full(ctx, id)) {
        return Err(ports::queue_full());
    }
    let bridge = web_bridge(ctx)?;
    let bytes = structured_clone::serialize(ctx, &message, &list, true, &bridge)?;
    let Some(id) = link.id.get() else {
        clone_transfer::abort_frame(ctx);
        return Ok(());
    };
    match ports::post_owned(ctx, id, bytes)? {
        Posted::Queued | Posted::Dropped | Posted::Lost => Ok(()),
        Posted::Full => Err(ports::queue_full()),
    }
}

// ---- the structured-clone bridge -----------------------------------------------------------------

/// The slot on a realm's global that holds its web bridge.
struct BridgeSlot(String);

/// Marks a closure's owner so the bridge can tell itself from another installed bridge.
fn global_bridge(ctx: &mut Ctx) -> Value {
    let global = ctx.global_object();
    ctx.member_get(&global, "__lumenPortClone")
        .unwrap_or(Value::Undefined)
}

/// Another installed bridge (Node's, when `worker_threads` is loaded) the web bridge hands
/// non-web ports to.
fn other_bridge(ctx: &mut Ctx, own: &Value) -> Option<Value> {
    let installed = global_bridge(ctx);
    (matches!(installed, Value::Obj(_)) && !crate::events::same(&installed, own)).then_some(installed)
}

fn delegate(ctx: &mut Ctx, own: &Value, name: &str, args: &[Value]) -> OpResult<Value> {
    let Some(other) = other_bridge(ctx, own) else {
        return Ok(Value::Undefined);
    };
    let function = ctx.member_get(&other, name).map_err(OpError::thrown)?;
    if !function.is_callable() {
        return Ok(Value::Undefined);
    }
    ctx.invoke(function, other, args).map_err(OpError::thrown)
}

fn native_port(ctx: &mut Ctx, value: &Value) -> Option<Rc<Link>> {
    ctx.with_instance::<bindings::MessagePort, _>(value, |port| port.link.clone())
        .ok()
}

/// The object the serializer asks about ports (`isPort`, `validate`, `export`, `detach`,
/// `import`, ...), for web ports. Ports of another bridge (Node's) are passed to it.
fn build_bridge(ctx: &mut Ctx) -> Value {
    let bridge = Value::Obj(ctx.new_object());
    let own = ctx.weak_value(&bridge).expect("the bridge is an object");
    let define = |ctx: &mut Ctx,
                      name: &'static str,
                      length: usize,
                      body: fn(&mut Ctx, &Value, &[Value]) -> OpResult<Value>| {
        let own = own.clone();
        let function = ctx.new_native_fn(
            name,
            length,
            Rc::new(move |ctx: &mut Ctx, _this: Value, args: &[Value]| {
                let Some(own) = own.upgrade() else {
                    return Ok(Value::Undefined);
                };
                body(ctx, &own, args).map_err(|error| error.to_value(ctx))
            }),
        );
        let _ = ctx.member_set(&bridge, name, function);
    };
    define(ctx, "isPort", 1, |ctx, own, args| {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        if native_port(ctx, &value).is_some() {
            return Ok(Value::Bool(true));
        }
        delegate(ctx, own, "isPort", &[value])
    });
    define(ctx, "isUntransferable", 1, |ctx, own, args| {
        delegate(ctx, own, "isUntransferable", args)
    });
    define(ctx, "isUncloneable", 1, |ctx, own, args| {
        delegate(ctx, own, "isUncloneable", args)
    });
    define(ctx, "validate", 1, |ctx, own, args| {
        let port = args.first().cloned().unwrap_or(Value::Undefined);
        let Some(link) = native_port(ctx, &port) else {
            return delegate(ctx, own, "validate", &[port]);
        };
        let open = match link.id.get() {
            Some(id) => !ports::is_closed(ctx, id).unwrap_or(true),
            None => false,
        };
        if open {
            Ok(Value::Undefined)
        } else {
            Err(NativeError::named(
                "DataCloneError",
                "MessagePort in transfer list is already detached",
            )
            .into())
        }
    });
    define(ctx, "export", 1, |ctx, own, args| {
        let port = args.first().cloned().unwrap_or(Value::Undefined);
        let Some(link) = native_port(ctx, &port) else {
            return delegate(ctx, own, "export", &[port]);
        };
        let id = link.id.get().ok_or_else(|| {
            NativeError::named("DataCloneError", "MessagePort is closed or detached")
        })?;
        Ok(Value::Num(ports::export(ctx, id)? as f64))
    });
    define(ctx, "detach", 1, |ctx, own, args| {
        let port = args.first().cloned().unwrap_or(Value::Undefined);
        let Some(link) = native_port(ctx, &port) else {
            return delegate(ctx, own, "detach", &[port]);
        };
        if let Some(id) = link.id.take() {
            ports::set_pin(ctx, id, None);
            ports::detach(ctx, id);
            link.on_close.borrow_mut().take();
        }
        Ok(Value::Undefined)
    });
    define(ctx, "import", 1, |ctx, _own, args| {
        let index = match args.first() {
            Some(Value::Num(index)) => *index,
            _ => f64::NAN,
        };
        let id = ports::import(ctx, index)?;
        let port = new_port(ctx, id, None)?;
        ports::note_received(ctx, &port);
        Ok(port)
    });
    bridge
}

/// This realm's web bridge, built on first use and kept on its global (traced, so the realm
/// stays collectable).
pub(crate) fn web_bridge(ctx: &mut Ctx) -> OpResult<Value> {
    let name = match ctx.op_state().get::<BridgeSlot>() {
        Some(slot) => slot.0.clone(),
        None => {
            let name = ctx.allocate_native_private_slot_name();
            ctx.op_state().put(BridgeSlot(name.clone()));
            name
        }
    };
    let global = ctx.global_object();
    if let Some(bridge) = ctx.native_private_value_slot(&global, &name) {
        return Ok(bridge);
    }
    let bridge = build_bridge(ctx);
    let _ = ctx.define_native_private_value_slot(&global, &name, bridge.clone());
    Ok(bridge)
}

/// Define `globalThis.__lumenPortClone` as the web bridge unless the realm already has one (Node
/// replaces it with its own when `worker_threads` loads).
pub fn install_port_clone(ctx: &mut Ctx) -> Result<(), Value> {
    let global = ctx.global_object();
    if !matches!(ctx.member_get(&global, "__lumenPortClone")?, Value::Undefined) {
        return Ok(());
    }
    let bridge = web_bridge(ctx).map_err(|error| error.to_value(ctx))?;
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (key, value) in [
        ("value", bridge),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(false)),
        ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, key, value)?;
    }
    ctx.define_property_value(&global, Value::str("__lumenPortClone"), &descriptor)
}

// ---- classes ---------------------------------------------------------------------------------

#[lumen_bind::module(name = "messageChannels")]
pub mod bindings {
    use super::*;
    use lumen::embed::JsHost;
    use lumen_bind::{CtorRet, Host, This};

    #[class(name = "MessagePort", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct MessagePort {
        pub(super) base: EventTarget,
        pub(super) link: Rc<Link>,
    }

    #[class(name = "MessageChannel", hint(js(webidl, invalid_this)))]
    pub struct MessageChannel {
        port1_slot: String,
        port2_slot: String,
    }

    #[class(name = "BroadcastChannel", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct BroadcastChannel {
        pub(super) base: EventTarget,
        pub(super) link: Rc<Link>,
        name: String,
    }

    // ---- MessagePort -------------------------------------------------------------------------

    #[methods]
    impl MessagePort {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(OpError::type_error("Illegal constructor").with_code("ERR_ILLEGAL_CONSTRUCTOR"))
        }

        #[method(name = "postMessage")]
        fn post_message(
            &self,
            ctx: &mut Ctx,
            message: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            if self.link.id.get().is_none() {
                return Ok(());
            }
            post_link(ctx, &self.link, message, options)
        }

        fn start(ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
            let Some(link) = link_of(ctx, &this.0) else {
                return Err(crate::webidl::invalid_this("MessagePort"));
            };
            start_link(ctx, &this.0, &link)
        }

        fn close(ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
            let Some(link) = link_of(ctx, &this.0) else {
                return Err(crate::webidl::invalid_this("MessagePort"));
            };
            close_link(ctx, &this.0, &link, Receiver::Port);
            Ok(())
        }

        #[getter]
        fn onmessage(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "message")
        }

        #[setter]
        fn set_onmessage(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            let callable = value.is_callable();
            node_handler_set(ctx, &this.0, "message", value)?;
            if callable {
                let Some(link) = link_of(ctx, &this.0) else {
                    return Ok(());
                };
                start_link(ctx, &this.0, &link)?;
            }
            Ok(())
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

    impl NativeIdentityOwner for MessagePort {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
            if let Some(hook) = &*self.link.on_close.borrow() {
                visit(hook);
            }
        }
    }

    // ---- MessageChannel ----------------------------------------------------------------------

    #[methods]
    impl MessageChannel {
        #[constructor]
        fn constructor(ctx: &mut Ctx) -> OpResult<Slotted<Self>> {
            require_ports(ctx)?;
            let (first, second) = ports::new_pair();
            let first = ports::adopt(ctx, first);
            let second = ports::adopt(ctx, second);
            let port1 = new_port(ctx, first, None)?;
            let port2 = new_port(ctx, second, None)?;
            let port1_slot = ctx.allocate_native_private_slot_name();
            let port2_slot = ctx.allocate_native_private_slot_name();
            Ok(Slotted {
                slots: vec![(port1_slot.clone(), port1), (port2_slot.clone(), port2)],
                value: Self {
                    port1_slot,
                    port2_slot,
                },
            })
        }

        #[getter]
        fn port1(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.port1_slot)
                .unwrap_or(Value::Undefined)
        }

        #[getter]
        fn port2(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.port2_slot)
                .unwrap_or(Value::Undefined)
        }
    }

    // ---- BroadcastChannel --------------------------------------------------------------------

    /// A `BroadcastChannel` that starts receiving as soon as it exists.
    pub struct BroadcastConstructor(BroadcastChannel);

    impl CtorRet<JsHost, BroadcastChannel> for BroadcastConstructor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let link = self.0.link.clone();
            let instance = <JsHost as Host>::construct(cx, self.0)?;
            <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                ctx.set_native_identity_owner::<BroadcastChannel>(&instance)
                    .and_then(|()| start_link(ctx, &instance, &link))
                    .map_err(|error| error.to_value(ctx))
            })?;
            Ok(instance)
        }
    }

    /// The origin that scopes channel names: `location.origin` when the realm has one.
    fn channel_origin(ctx: &mut Ctx) -> String {
        let global = ctx.global_object();
        let origin = match ctx.member_get(&global, "location") {
            Ok(location @ Value::Obj(_)) => ctx.member_get(&location, "origin").ok(),
            _ => None,
        };
        match origin {
            Some(Value::Str(origin)) => origin.to_string(),
            _ => String::new(),
        }
    }

    #[methods]
    impl BroadcastChannel {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"name\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, name: &str) -> OpResult<BroadcastConstructor> {
            require_ports(ctx)?;
            let key = format!("{}\0{name}", channel_origin(ctx));
            let id = ports::broadcast(ctx, key);
            Ok(BroadcastConstructor(Self {
                base: new_target(),
                link: Link::new(ctx, id, None),
                name: name.into(),
            }))
        }

        #[getter]
        fn name(&self) -> String {
            self.name.clone()
        }

        #[method(name = "postMessage")]
        fn post_message(&self, ctx: &mut Ctx, message: Value) -> OpResult<()> {
            if self.link.id.get().is_none() {
                return Err(NativeError::named(
                    "InvalidStateError",
                    "BroadcastChannel is closed",
                )
                .into());
            }
            post_link(ctx, &self.link, message, Value::Undefined)
        }

        fn close(ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
            let Some(link) = link_of(ctx, &this.0) else {
                return Err(crate::webidl::invalid_this("BroadcastChannel"));
            };
            close_link(ctx, &this.0, &link, Receiver::Port);
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
    }

    impl NativeIdentityOwner for BroadcastChannel {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }
}

/// The hidden `__lumenSharedPorts` namespace: wraps an endpoint a native op adopted (a shared
/// worker's connection) as a web `MessagePort`.
pub mod shared {
    use super::*;

    #[lumen_bind::module(name = "__lumenSharedPorts")]
    mod natives {
        use super::*;

        /// `create(id, onClose)`: a `MessagePort` around the adopted endpoint `id`. `onClose`
        /// runs once when the endpoint closes.
        #[op(name = "create", coerce)]
        fn create(ctx: &mut Ctx, id: f64, on_close: Option<Value>) -> OpResult<Value> {
            require_ports(ctx)?;
            new_port(ctx, id as u64, on_close.filter(Value::is_callable))
        }
    }

    pub use natives::Module;
}
