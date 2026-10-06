//! The `EventSource` class (HTML Standard, "Server-sent events") over the stream transport in
//! `sse.rs`.
//!
//! The transport streams the `text/event-stream` body to a dispatch function the class creates
//! for each connection: `("open")`, `("chunk", u8array)`, `("drop", message)` (reconnect),
//! `("fatal", message)` (fail permanently) and `("closed")`. That function holds the instance
//! weakly. A source that has a listener and is not closed is pinned, including while it waits to
//! reconnect; any other source is collectable.

use crate::net_class::{
    dom_error, fire, fire_plain, has_listener, new_pin_key, parse_url, set_pin, text_arg,
    Connection, Started,
};
use crate::sse::{close_stream, connect_stream};
use lumen::embed::{Ctx, NativeIdentityOwner, OpError, OpResult, Value};
use lumen_common::encoding::TextDecoder;
use lumen_host::events::{node_handler_get, node_handler_set, EventTarget, TargetData};
use lumen_host::messaging::MessageEvent;
use lumen_host::timers::{set_timeout, Timer};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const STATE_CONNECTING: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_CLOSED: u8 = 2;

const DEFAULT_RETRY_MS: f64 = 3000.0;

/// What the line parser found in a chunk.
#[derive(Debug, PartialEq)]
enum Action {
    /// A `retry` field: the new reconnection time in milliseconds.
    Retry(u64),
    /// An empty line: `id` is the new last event id; `message` is `(type, data)` when the block
    /// carried any data.
    Dispatch {
        id: String,
        message: Option<(String, String)>,
    },
}

/// The stream parser of the standard: decodes UTF-8 across chunk boundaries, splits lines
/// (LF, CR or CRLF, also across chunks) and groups fields into events.
struct Parser {
    decoder: TextDecoder,
    pending: String,
    skip_lf: bool,
    data: String,
    event_type: String,
    id_buffer: String,
}

impl Parser {
    /// A parser for a new connection; the id buffer starts as the last event id, which persists
    /// across reconnections.
    fn new(last_event_id: &str) -> Self {
        Self {
            decoder: TextDecoder::new("utf-8", false, false).expect("utf-8 is a known label"),
            pending: String::new(),
            skip_lf: false,
            data: String::new(),
            event_type: String::new(),
            id_buffer: last_event_id.into(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let text = self.decoder.decode(bytes, true).unwrap_or_default();
        self.pending.push_str(&text);
        if self.skip_lf && !self.pending.is_empty() {
            if self.pending.starts_with('\n') {
                self.pending.remove(0);
            }
            self.skip_lf = false;
        }
        let mut lines = Vec::new();
        let mut start = 0;
        let mut index = 0;
        let mut ends_with_cr = false;
        let pending = self.pending.as_bytes();
        while index < pending.len() {
            match pending[index] {
                b'\n' => {
                    lines.push(start..index);
                    index += 1;
                    start = index;
                }
                b'\r' => {
                    lines.push(start..index);
                    index += 1;
                    match pending.get(index) {
                        Some(b'\n') => index += 1,
                        None => ends_with_cr = true,
                        Some(_) => {}
                    }
                    start = index;
                }
                _ => index += 1,
            }
        }
        self.skip_lf = ends_with_cr;
        let pending = std::mem::take(&mut self.pending);
        let mut actions = Vec::new();
        for range in lines {
            self.line(&pending[range], &mut actions);
        }
        self.pending = pending[start..].to_string();
        actions
    }

    fn line(&mut self, line: &str, actions: &mut Vec<Action>) {
        if line.is_empty() {
            self.dispatch(actions);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event_type = value.into(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "id" if !value.contains('\0') => self.id_buffer = value.into(),
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                actions.push(Action::Retry(value.parse().unwrap_or(u64::MAX)));
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, actions: &mut Vec<Action>) {
        let id = self.id_buffer.clone();
        let event_type = std::mem::take(&mut self.event_type);
        if self.data.is_empty() {
            actions.push(Action::Dispatch { id, message: None });
            return;
        }
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        let kind = if event_type.is_empty() { "message".into() } else { event_type };
        actions.push(Action::Dispatch {
            id,
            message: Some((kind, data)),
        });
    }
}

struct SourceState {
    url: String,
    origin: String,
    with_credentials: bool,
    ready: Cell<u8>,
    id: Cell<Option<u64>>,
    retry_ms: Cell<f64>,
    last_event_id: RefCell<String>,
    timer: RefCell<Option<Timer>>,
    parser: RefCell<Parser>,
    pin: u64,
}

fn state_of(ctx: &mut Ctx, receiver: &Value) -> Option<Rc<SourceState>> {
    ctx.with_instance::<bindings::EventSource, _>(receiver, |source| source.state.clone())
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

fn refresh_pin(ctx: &mut Ctx, receiver: &Value, state: &SourceState) {
    let keep = state.ready.get() != STATE_CLOSED && has_listener(ctx, receiver, None);
    set_pin(ctx, state.pin, keep.then(|| receiver.clone()));
}

/// Open a connection resuming from the last event id.
fn connect(ctx: &mut Ctx, receiver: &Value, state: &SourceState) -> OpResult<()> {
    let last_event_id = state.last_event_id.borrow().clone();
    *state.parser.borrow_mut() = Parser::new(&last_event_id);
    let weak = ctx.weak_value(receiver).expect("sources are objects");
    let id = Rc::new(Cell::new(None::<u64>));
    let owned_id = id.clone();
    let dispatch = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |ctx: &mut Ctx, _this: Value, args: &[Value]| {
            match weak.upgrade() {
                Some(source) => on_event(ctx, &source, args),
                None => {
                    if let Some(id) = owned_id.get() {
                        close_stream(ctx, id);
                    }
                }
            }
            Ok(Value::Undefined)
        }),
    );
    let connected = connect_stream(ctx, &state.url, &last_event_id, dispatch)?;
    id.set(Some(connected));
    state.id.set(Some(connected));
    Ok(())
}

impl Connection for bindings::EventSource {
    fn start(ctx: &mut Ctx, receiver: &Value) -> OpResult<()> {
        let state = state_of(ctx, receiver).expect("an EventSource was just constructed");
        connect(ctx, receiver, &state)
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
            state.ready.set(STATE_OPEN);
            fire_plain(ctx, receiver, "open");
        }
        "chunk" if state.ready.get() == STATE_OPEN => {
            let bytes = args
                .get(1)
                .and_then(|chunk| ctx.buffer_source_bytes(chunk))
                .unwrap_or_default();
            let actions = state.parser.borrow_mut().feed(&bytes);
            for action in actions {
                if state.ready.get() == STATE_CLOSED {
                    break;
                }
                apply(ctx, receiver, &state, action);
            }
        }
        "drop" => reconnect_later(ctx, receiver, &state),
        "fatal" => {
            state.ready.set(STATE_CLOSED);
            state.id.set(None);
            refresh_pin(ctx, receiver, &state);
            fire_plain(ctx, receiver, "error");
        }
        _ => {}
    }
    refresh_pin(ctx, receiver, &state);
}

fn apply(ctx: &mut Ctx, receiver: &Value, state: &SourceState, action: Action) {
    match action {
        Action::Retry(ms) => state.retry_ms.set(ms as f64),
        Action::Dispatch { id, message } => {
            *state.last_event_id.borrow_mut() = id.clone();
            let Some((kind, data)) = message else {
                return;
            };
            let event = MessageEvent::create_with_id(ctx, &kind, Value::from_string(data), &state.origin, &id);
            fire(ctx, receiver, event);
        }
    }
}

/// The connection dropped: report it and try again after the reconnection time, unless the
/// `error` handler closed the source.
fn reconnect_later(ctx: &mut Ctx, receiver: &Value, state: &Rc<SourceState>) {
    state.ready.set(STATE_CONNECTING);
    state.id.set(None);
    fire_plain(ctx, receiver, "error");
    if state.ready.get() == STATE_CLOSED {
        return;
    }
    let weak = ctx.weak_value(receiver).expect("sources are objects");
    let timer = set_timeout(ctx, state.retry_ms.get(), move |ctx| {
        if let Some(source) = weak.upgrade() {
            reconnect(ctx, &source);
        }
    });
    match timer {
        Ok(timer) => *state.timer.borrow_mut() = Some(timer),
        Err(_) => {
            state.ready.set(STATE_CLOSED);
            refresh_pin(ctx, receiver, state);
        }
    }
}

fn reconnect(ctx: &mut Ctx, receiver: &Value) {
    let Some(state) = state_of(ctx, receiver) else {
        return;
    };
    state.timer.borrow_mut().take();
    if state.ready.get() == STATE_CLOSED {
        return;
    }
    if connect(ctx, receiver, &state).is_err() {
        state.ready.set(STATE_CLOSED);
        refresh_pin(ctx, receiver, &state);
        fire_plain(ctx, receiver, "error");
    }
}

#[lumen_bind::module(name = "eventSource")]
pub mod bindings {
    use super::*;
    use lumen_bind::This;

    #[class(name = "EventSource", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct EventSource {
        pub(super) base: EventTarget,
        pub(super) state: Rc<SourceState>,
    }

    #[methods]
    impl EventSource {
        #[constant(name = "CONNECTING")]
        const CONNECTING: u16 = 0;
        #[constant(name = "OPEN")]
        const OPEN: u16 = 1;
        #[constant(name = "CLOSED")]
        const CLOSED: u16 = 2;

        #[constructor(coerce)]
        fn constructor(
            ctx: &mut Ctx,
            url: &str,
            #[default(Value::Undefined)] init: Value,
        ) -> OpResult<Started<Self>> {
            let Some(parsed) = parse_url(ctx, url) else {
                return Err(dom_error(ctx, "SyntaxError", format!("invalid EventSource URL: {url}")));
            };
            let with_credentials = match &init {
                Value::Obj(_) => {
                    let flag = ctx.member_get(&init, "withCredentials").map_err(OpError::thrown)?;
                    ctx.to_boolean(&flag)
                }
                _ => false,
            };
            let pin = new_pin_key(ctx);
            Ok(Started(Self {
                base: new_target(),
                state: Rc::new(SourceState {
                    url: parsed.href,
                    origin: parsed.origin,
                    with_credentials,
                    ready: Cell::new(STATE_CONNECTING),
                    id: Cell::new(None),
                    retry_ms: Cell::new(DEFAULT_RETRY_MS),
                    last_event_id: RefCell::new(String::new()),
                    timer: RefCell::new(None),
                    parser: RefCell::new(Parser::new("")),
                    pin,
                }),
            }))
        }

        #[getter]
        fn url(&self) -> String {
            self.state.url.clone()
        }

        #[getter]
        fn with_credentials(&self) -> bool {
            self.state.with_credentials
        }

        #[getter]
        fn ready_state(&self) -> u16 {
            self.state.ready.get().into()
        }

        fn close(&self, ctx: &mut Ctx) {
            let state = &self.state;
            state.ready.set(STATE_CLOSED);
            let timer = state.timer.borrow_mut().take();
            if let Some(timer) = timer {
                timer.clear(ctx);
            }
            if let Some(id) = state.id.take() {
                close_stream(ctx, id);
            }
            set_pin(ctx, state.pin, None);
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
    }

    impl NativeIdentityOwner for EventSource {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data().trace_callbacks(visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: &str, data: &str, id: &str) -> Action {
        Action::Dispatch {
            id: id.into(),
            message: Some((kind.into(), data.into())),
        }
    }

    #[test]
    fn parses_fields_comments_and_multiline_data() {
        let mut parser = Parser::new("");
        let actions = parser.feed(b": comment\nevent: tick\ndata: a\ndata\ndata:  b\nid: 7\n\ndata: x\n\n");
        assert_eq!(
            actions,
            [message("tick", "a\n\n b", "7"), message("message", "x", "7")]
        );
    }

    #[test]
    fn line_endings_and_chunk_splits() {
        let mut parser = Parser::new("");
        let mut actions = parser.feed(b"data: one\r");
        actions.extend(parser.feed(b"\ndata: two\r\n\r"));
        actions.extend(parser.feed(b"\n"));
        assert_eq!(actions, [message("message", "one\ntwo", "")]);
    }

    #[test]
    fn utf8_split_across_chunks_and_bom() {
        let mut parser = Parser::new("");
        let bytes = "\u{feff}data: caf\u{e9}\n\n".as_bytes().to_vec();
        let split = bytes.iter().position(|&byte| byte == 0xc3).unwrap() + 1;
        let mut actions = parser.feed(&bytes[..split]);
        actions.extend(parser.feed(&bytes[split..]));
        assert_eq!(actions, [message("message", "caf\u{e9}", "")]);
    }

    #[test]
    fn id_with_nul_is_ignored_and_ids_persist() {
        let mut parser = Parser::new("seed");
        let actions = parser.feed(b"id: a\0b\ndata: 1\n\nid: 5\n\ndata: 2\n\n");
        assert_eq!(
            actions,
            [
                message("message", "1", "seed"),
                Action::Dispatch { id: "5".into(), message: None },
                message("message", "2", "5"),
            ]
        );
    }

    #[test]
    fn retry_accepts_only_digits() {
        let mut parser = Parser::new("");
        let actions = parser.feed(b"retry: 250\nretry: 1x\nretry:\n");
        assert_eq!(actions, [Action::Retry(250)]);
    }
}
