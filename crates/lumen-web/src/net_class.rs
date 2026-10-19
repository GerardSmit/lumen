//! Pieces the native `WebSocket` and `EventSource` classes share: the constructor that starts the
//! connection once the wrapper exists, the strong references that keep a listening connection
//! alive, native URL parsing against the shared API base, and event helpers.

use lumen::embed::{Ctx, JsHost, NativeIdentityOwner, OpResult, Value};
use lumen_bind::{Class, CtorRet, Host};
use lumen_host::events::{Event, EventTarget};
use std::collections::HashMap;

/// A class whose instances open a connection as soon as they exist.
pub(crate) trait Connection: Class + NativeIdentityOwner + 'static {
    /// Open the connection for the freshly constructed `receiver`.
    fn start(ctx: &mut Ctx, receiver: &Value) -> OpResult<()>;
}

/// A constructor result that makes the instance a native identity owner and starts it. The
/// transport's dispatch function refers to the instance weakly, so it can only be built once the
/// wrapper exists.
pub(crate) struct Started<T>(pub(crate) T);

impl<T: Connection> CtorRet<JsHost, T> for Started<T> {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            ctx.set_native_identity_owner::<T>(&instance)
                .and_then(|()| T::start(ctx, &instance))
                .map_err(|error| error.to_value(ctx))
        })?;
        Ok(instance)
    }
}

#[derive(Default)]
struct Pins {
    next: u64,
    held: HashMap<u64, Value>,
}

fn pins(ctx: &mut Ctx) -> &mut Pins {
    if !ctx.op_state().has::<Pins>() {
        ctx.op_state().put(Pins::default());
    }
    ctx.host_mut::<Pins>().expect("installed above")
}

/// A key for [`set_pin`].
pub(crate) fn new_pin_key(ctx: &mut Ctx) -> u64 {
    let pins = pins(ctx);
    pins.next += 1;
    pins.next
}

/// Hold `pin` (the connection's wrapper) alive until it is cleared. A pinned connection is
/// reachable without a script reference, so it can still deliver events to its listeners.
pub(crate) fn set_pin(ctx: &mut Ctx, key: u64, pin: Option<Value>) {
    let pins = pins(ctx);
    match pin {
        Some(value) => {
            pins.held.insert(key, value);
        }
        None => {
            pins.held.remove(&key);
        }
    }
}

pub(crate) use lumen_host::net::dom_error;

/// A URL record serialized by the common URL authority.
pub(crate) struct ParsedUrl {
    pub(crate) href: String,
    pub(crate) protocol: String,
    pub(crate) hash: String,
    pub(crate) origin: String,
}

/// Parse a scalar URL against the relevant settings object's API base URL.
pub(crate) fn parse_url(ctx: &mut Ctx, text: &str) -> Option<ParsedUrl> {
    let base=lumen_host::net::api_base_url(ctx);
    let text=lumen::well_formed_utf8(text);
    let url=lumen_common::url::parse(&text,base.as_deref()).ok()?;
    Some(ParsedUrl {
        href: url.href(),
        protocol: format!("{}:",url.scheme),
        hash: url.fragment.as_deref().filter(|fragment|!fragment.is_empty())
            .map_or_else(String::new,|fragment|format!("#{fragment}")),
        origin: url.origin(),
    })
}

/// Fire a trusted event on `receiver`; a throwing handler is reported by the dispatch.
pub(crate) fn fire(ctx: &mut Ctx, receiver: &Value, event: OpResult<Value>) {
    if let Ok(event) = event {
        let _ = EventTarget::dispatch_trusted(ctx, receiver, &event);
    }
}

/// Fire a trusted plain `Event` of type `kind`.
pub(crate) fn fire_plain(ctx: &mut Ctx, receiver: &Value, kind: &str) {
    let event = ctx.new_instance(Event::trusted(kind));
    fire(ctx, receiver, Ok(event));
}

/// Whether `receiver` has a live listener or handler for any of `kinds`; every kind when `kinds`
/// is `None`.
pub(crate) fn has_listener(ctx: &mut Ctx, receiver: &Value, kinds: Option<&[&str]>) -> bool {
    let Ok((data, _)) = EventTarget::of_receiver(ctx, receiver) else {
        return false;
    };
    match kinds {
        Some(kinds) => kinds.iter().any(|kind| data.listener_count(kind) > 0),
        None => data
            .event_names()
            .iter()
            .any(|kind| data.listener_count(kind) > 0),
    }
}

/// The string at `args[index]` of a transport dispatch (`""` when absent).
pub(crate) fn text_arg(args: &[Value], index: usize) -> String {
    match args.get(index) {
        Some(Value::Str(text)) => text.to_string(),
        _ => String::new(),
    }
}

/// The number at `args[index]` of a transport dispatch.
pub(crate) fn number_arg(args: &[Value], index: usize) -> Option<f64> {
    match args.get(index) {
        Some(Value::Num(number)) => Some(*number),
        _ => None,
    }
}
