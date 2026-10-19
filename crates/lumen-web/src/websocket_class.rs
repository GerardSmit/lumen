//! The `WebSocket` class (HTML Standard, "The WebSocket interface") over the transport in
//! `websocket.rs`.
//!
//! The transport owns the socket and reports its lifecycle through a dispatch function the class
//! creates when it starts: `("open", protocol)`, `("text", string)`, `("binary", u8array)`,
//! `("close", code, reason, wasClean)`, `("fail", code, message)`, `("error" | "io", message)`.
//! That function holds the instance weakly. A socket that has a listener and is not closed is
//! pinned, so it keeps delivering events without a script reference; any other socket is
//! collectable, and when its instance is gone the transport closes it and stops reading.

use crate::net_class::{
    dom_error, fire, fire_plain, has_listener, new_pin_key, number_arg, parse_url, set_pin,
    text_arg, Connection, Started,
};
use crate::websocket::{abandon_socket, close_socket, connect_socket, send_frame};
use lumen::embed::{Ctx, NativeIdentityOwner, OpError, OpResult, Value};
use lumen_host::events::{node_handler_get, node_handler_set, EventTarget, TargetData};
use lumen_host::messaging::{CloseEvent, MessageEvent};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// An outgoing message.
pub(crate) enum Outgoing<'a> {
    Text(&'a str),
    Binary(&'a [u8]),
}

const STATE_CONNECTING: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_CLOSING: u8 = 2;
const STATE_CLOSED: u8 = 3;

const EVENTS: [&str; 4] = ["open", "message", "error", "close"];

/// What the transport reports when a close frame carries no status code.
const NO_STATUS: f64 = 1005.0;
const ABNORMAL_CLOSURE: u16 = 1006;

struct SocketState {
    url: String,
    protocols: Vec<String>,
    ready: Cell<u8>,
    blob_messages: Cell<bool>,
    protocol: RefCell<String>,
    id: Cell<Option<u64>>,
    pin: u64,
}

fn state_of(ctx: &mut Ctx, receiver: &Value) -> Option<Rc<SocketState>> {
    ctx.with_instance::<bindings::WebSocket, _>(receiver, |socket| socket.state.clone())
        .ok()
}

fn new_target() -> EventTarget {
    let base = EventTarget::from_data(TargetData::new(None));
    base.data().observe_changes(listeners_changed);
    base
}

fn listeners_changed(ctx: &mut Ctx, receiver: &Value, _: &TargetData) {
    if let Some(state) = state_of(ctx, receiver) {
        refresh_pin(ctx, receiver, &state);
    }
}

fn refresh_pin(ctx: &mut Ctx, receiver: &Value, state: &SocketState) {
    let keep = state.ready.get() != STATE_CLOSED && has_listener(ctx, receiver, Some(&EVENTS));
    set_pin(ctx, state.pin, keep.then(|| receiver.clone()));
}

/// A subprotocol token must be a non-empty HTTP token (RFC 7230): visible ASCII minus separators.
fn is_valid_protocol(protocol: &str) -> bool {
    !protocol.is_empty()
        && protocol
            .chars()
            .all(|c| ('\u{21}'..='\u{7e}').contains(&c) && !"()<>@,;:\\\"/[]?={}".contains(c))
}

fn read_protocols(ctx: &mut Ctx, value: Value) -> OpResult<Vec<String>> {
    let items = match value {
        Value::Undefined => Vec::new(),
        value if ctx.is_array_value(&value).map_err(OpError::thrown)? => {
            ctx.iterable_to_list(&value, usize::MAX)?
        }
        value => vec![value],
    };
    let mut protocols: Vec<String> = Vec::new();
    for item in &items {
        let protocol = ctx.coerce_string(item).map_err(OpError::thrown)?.to_string();
        if !is_valid_protocol(&protocol) || protocols.contains(&protocol) {
            return Err(dom_error(
                ctx,
                "SyntaxError",
                "invalid or duplicate WebSocket subprotocol",
            ));
        }
        protocols.push(protocol);
    }
    Ok(protocols)
}

impl Connection for bindings::WebSocket {
    fn start(ctx: &mut Ctx, receiver: &Value) -> OpResult<()> {
        let state = state_of(ctx, receiver).expect("a WebSocket was just constructed");
        let weak = ctx.weak_value(receiver).expect("sockets are objects");
        let id = Rc::new(Cell::new(None::<u64>));
        let owned_id = id.clone();
        let dispatch = ctx.new_native_fn(
            "",
            0,
            Rc::new(move |ctx: &mut Ctx, _this: Value, args: &[Value]| {
                match weak.upgrade() {
                    Some(socket) => on_event(ctx, &socket, args),
                    None => {
                        if let Some(id) = owned_id.get() {
                            abandon_socket(ctx, id);
                        }
                    }
                }
                Ok(Value::Undefined)
            }),
        );
        let connected = connect_socket(ctx, &state.url, &state.protocols, dispatch)?;
        id.set(Some(connected));
        state.id.set(Some(connected));
        Ok(())
    }
}

/// One report from the transport.
fn on_event(ctx: &mut Ctx, receiver: &Value, args: &[Value]) {
    let Some(state) = state_of(ctx, receiver) else {
        return;
    };
    if state.ready.get() == STATE_CLOSED {
        return;
    }
    match text_arg(args, 0).as_str() {
        "open" => {
            if state.ready.get() == STATE_CLOSING {
                if let Some(id) = state.id.get() {
                    abandon_socket(ctx, id);
                }
                fail(ctx, receiver, &state, ABNORMAL_CLOSURE, "");
                return;
            }
            state.ready.set(STATE_OPEN);
            *state.protocol.borrow_mut() = text_arg(args, 1);
            fire_plain(ctx, receiver, "open");
        }
        "text" if state.ready.get() == STATE_OPEN => {
            let data = args.get(1).cloned().unwrap_or(Value::Undefined);
            let event = MessageEvent::create(ctx, "message", data, &state.url, Value::Null, Vec::new());
            fire(ctx, receiver, event);
        }
        "binary" if state.ready.get() == STATE_OPEN => {
            let bytes = args
                .get(1)
                .and_then(|view| ctx.buffer_source_bytes(view))
                .unwrap_or_default();
            let data = if state.blob_messages.get() {
                lumen_host::blob::new_blob(ctx, bytes, "")
            } else {
                ctx.make_array_buffer_from(bytes)
            };
            let event = MessageEvent::create(ctx, "message", data, &state.url, Value::Null, Vec::new());
            fire(ctx, receiver, event);
        }
        "close" => {
            let code = number_arg(args, 1).unwrap_or(NO_STATUS) as u16;
            let clean = matches!(args.get(3), Some(Value::Bool(true)));
            finish(ctx, receiver, &state, code, &text_arg(args, 2), clean);
        }
        "fail" => {
            let code = number_arg(args, 1).map_or(ABNORMAL_CLOSURE, |code| code as u16);
            fail(ctx, receiver, &state, code, &text_arg(args, 2));
        }
        "error" | "io" => fail(ctx, receiver, &state, ABNORMAL_CLOSURE, ""),
        _ => {}
    }
    refresh_pin(ctx, receiver, &state);
}

/// The connection is over: release the pin, then fire `close`.
fn finish(ctx: &mut Ctx, receiver: &Value, state: &SocketState, code: u16, reason: &str, clean: bool) {
    state.ready.set(STATE_CLOSED);
    state.id.set(None);
    refresh_pin(ctx, receiver, state);
    let event = CloseEvent::create(ctx, "close", code, reason, clean);
    fire(ctx, receiver, Ok(event));
}

/// A failed connection: an `error` event, then a non-clean `close`.
fn fail(ctx: &mut Ctx, receiver: &Value, state: &SocketState, code: u16, reason: &str) {
    state.ready.set(STATE_CLOSED);
    state.id.set(None);
    refresh_pin(ctx, receiver, state);
    fire_plain(ctx, receiver, "error");
    let event = CloseEvent::create(ctx, "close", code, reason, false);
    fire(ctx, receiver, Ok(event));
}

#[lumen_bind::module(name = "webSocket")]
pub mod bindings {
    use super::*;
    use lumen_bind::{NativeError, This};

    #[class(name = "WebSocket", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct WebSocket {
        pub(super) base: EventTarget,
        pub(super) state: Rc<SocketState>,
    }

    #[methods]
    impl WebSocket {
        #[constant(name = "CONNECTING")]
        const CONNECTING: u16 = 0;
        #[constant(name = "OPEN")]
        const OPEN: u16 = 1;
        #[constant(name = "CLOSING")]
        const CLOSING: u16 = 2;
        #[constant(name = "CLOSED")]
        const CLOSED: u16 = 3;

        #[constructor(coerce)]
        fn constructor(
            ctx: &mut Ctx,
            url: &str,
            #[default(Value::Undefined)] protocols: Value,
        ) -> OpResult<Started<Self>> {
            let Some(parsed) = parse_url(ctx, url) else {
                return Err(dom_error(ctx, "SyntaxError", format!("invalid WebSocket URL: {url}")));
            };
            if parsed.protocol != "ws:" && parsed.protocol != "wss:" {
                return Err(dom_error(
                    ctx,
                    "SyntaxError",
                    format!("WebSocket URL scheme must be ws or wss, got '{}'", parsed.protocol),
                ));
            }
            if !parsed.hash.is_empty() {
                return Err(dom_error(
                    ctx,
                    "SyntaxError",
                    "WebSocket URL must not contain a fragment",
                ));
            }
            let protocols = read_protocols(ctx, protocols)?;
            let pin = new_pin_key(ctx);
            Ok(Started(Self {
                base: new_target(),
                state: Rc::new(SocketState {
                    url: parsed.href,
                    protocols,
                    ready: Cell::new(STATE_CONNECTING),
                    blob_messages: Cell::new(true),
                    protocol: RefCell::new(String::new()),
                    id: Cell::new(None),
                    pin,
                }),
            }))
        }

        #[getter]
        fn url(&self) -> String {
            self.state.url.clone()
        }

        #[getter]
        fn ready_state(&self) -> u16 {
            self.state.ready.get().into()
        }

        #[getter]
        fn buffered_amount(&self) -> u32 {
            0
        }

        #[getter]
        fn extensions(&self) -> String {
            String::new()
        }

        #[getter]
        fn protocol(&self) -> String {
            self.state.protocol.borrow().clone()
        }

        #[getter]
        fn binary_type(&self) -> &'static str {
            if self.state.blob_messages.get() {
                "blob"
            } else {
                "arraybuffer"
            }
        }

        #[setter]
        fn set_binary_type(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
            let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
            match &*value {
                "blob" => self.state.blob_messages.set(true),
                "arraybuffer" => self.state.blob_messages.set(false),
                _ => {
                    return Err(
                        NativeError::named("SyntaxError", "binaryType must be 'blob' or 'arraybuffer'")
                            .into(),
                    )
                }
            }
            Ok(())
        }

        fn send(&self, ctx: &mut Ctx, data: Value) -> OpResult<()> {
            match self.state.ready.get() {
                STATE_CONNECTING => {
                    return Err(
                        NativeError::named("InvalidStateError", "WebSocket is still CONNECTING")
                            .into(),
                    )
                }
                STATE_OPEN => {}
                _ => return Ok(()),
            }
            let Some(id) = self.state.id.get() else {
                return Ok(());
            };
            if let Some(bytes) = ctx.buffer_source_bytes(&data) {
                send_frame(ctx, id, Outgoing::Binary(&bytes))?;
            } else if let Some(blob) = lumen_host::blob::blob_of(ctx, &data) {
                let bytes = blob.source.bytes(ctx)?;
                send_frame(ctx, id, Outgoing::Binary(&bytes))?;
            } else {
                let text = ctx.coerce_string(&data).map_err(OpError::thrown)?;
                send_frame(ctx, id, Outgoing::Text(&text))?;
            }
            Ok(())
        }

        #[method(coerce)]
        fn close(
            &self,
            ctx: &mut Ctx,
            code: Option<f64>,
            reason: Option<String>,
        ) -> OpResult<()> {
            if let Some(code) = code {
                if code != 1000.0 && !(3000.0..=4999.0).contains(&code) {
                    return Err(NativeError::named(
                        "InvalidAccessError",
                        "close code must be 1000 or in 3000-4999",
                    )
                    .into());
                }
            }
            let reason = reason.unwrap_or_default();
            if reason.len() > 123 {
                return Err(dom_error(ctx, "SyntaxError", "close reason too long (>123 bytes)"));
            }
            let state = &self.state;
            match state.ready.get() {
                STATE_CLOSING | STATE_CLOSED => {}
                STATE_CONNECTING => state.ready.set(STATE_CLOSING),
                _ => {
                    state.ready.set(STATE_CLOSING);
                    if let Some(id) = state.id.get() {
                        close_socket(ctx, id, code.map_or(1000, |code| code as u16), &reason);
                    }
                }
            }
            Ok(())
        }

        #[getter]
        fn onopen(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "open")
        }

        #[setter]
        fn set_onopen(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "open", value)
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
        fn onerror(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "error")
        }

        #[setter]
        fn set_onerror(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "error", value)
        }

        #[getter]
        fn onclose(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            node_handler_get(ctx, &this.0, "close")
        }

        #[setter]
        fn set_onclose(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            node_handler_set(ctx, &this.0, "close", value)
        }
    }

    impl NativeIdentityOwner for WebSocket {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_valid_protocol;

    #[test]
    fn subprotocol_tokens() {
        assert!(is_valid_protocol("chat"));
        assert!(is_valid_protocol("v2.json-rpc"));
        assert!(!is_valid_protocol(""));
        assert!(!is_valid_protocol("bad proto"));
        assert!(!is_valid_protocol("a,b"));
        assert!(!is_valid_protocol("caf\u{e9}"));
    }
}
