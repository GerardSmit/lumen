//! The seam between the runtime and a browser embedding (`lumen-wasm`).
//!
//! The embedder installs one JS *host object* ([`set_host`], wasm32 only) whose methods the
//! native ops call into: `fetch`, `wsOpen`/`wsSend`/`wsClose`, `syncCall`. Results come back the
//! other way as [`Event`]s pushed into the loop's completion queue (a settled Promise becomes
//! one `Event`), so the runtime never blocks on the browser and never needs a thread.
//!
//! [`sync_call`] is the *suspending* primitive: a native op that must look synchronous to the
//! script (a sync file read that is really a network or OPFS access) calls it, and the host's
//! `syncCall` method blocks the guest until the answer exists — with `Atomics.wait` over a
//! `SharedArrayBuffer` while a helper Worker awaits the Promise, or by suspending the wasm stack
//! under JSPI. Run-to-completion holds: nothing else of the realm runs meanwhile. Natively there
//! is no host; tests can install a handler with [`set_sync_handler`].

use std::any::Any;
use std::sync::OnceLock;

use crate::{Ctx, Value};
use lumen_bind::{Data, IntoRet, NativeError};

/// One value crossing from the browser into the realm: a neutral [`Data`] or an explicit `null`
/// (`Data::None` is `undefined`).
#[derive(Debug, Clone)]
pub enum Arg {
    Null,
    Data(Data),
}

impl<T: Into<Data>> From<T> for Arg {
    fn from(v: T) -> Arg {
        Arg::Data(v.into())
    }
}

/// The error of an API with no browser counterpart (`code` `ERR_NOT_SUPPORTED_IN_BROWSER`).
pub fn unsupported(what: &str) -> NativeError {
    NativeError::runtime(format!("{what} is not supported in the browser"))
        .with_code("ERR_NOT_SUPPORTED_IN_BROWSER")
}

/// A message from the browser to a registered task (a fetch response, a WebSocket event):
/// `kind` names what happened and `args` carry its data.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: String,
    pub args: Vec<Arg>,
}

impl Event {
    pub fn new(kind: impl Into<String>, args: Vec<Arg>) -> Event {
        Event {
            kind: kind.into(),
            args,
        }
    }
}

/// An [`Arg`] as the JS value it stands for (bytes become a `Uint8Array`).
pub fn arg_value(ctx: &mut Ctx, arg: Arg) -> Result<Value, Value> {
    match arg {
        Arg::Null => Ok(Value::Null),
        Arg::Data(d) => IntoRet::<lumen::embed::JsHost>::into_ret(d, ctx),
    }
}

/// A [`crate::TaskDecoder`] that hands the event to the task's callback as `(kind, ...args)` —
/// the shape the WebSocket glue's `dispatch` callback expects.
pub fn decode_event(ctx: &mut Ctx, payload: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
    let event = *payload.downcast::<Event>().expect("browser event payload");
    let mut out = vec![Value::from_string(event.kind)];
    for arg in event.args {
        out.push(arg_value(ctx, arg)?);
    }
    Ok(out)
}

type SyncHandler = Box<dyn Fn(&str, &[u8]) -> Result<Vec<u8>, String> + Send + Sync>;
static SYNC_HANDLER: OnceLock<SyncHandler> = OnceLock::new();

/// Install the suspending-call implementation where there is no JS host (native tests).
pub fn set_sync_handler(
    f: impl Fn(&str, &[u8]) -> Result<Vec<u8>, String> + Send + Sync + 'static,
) {
    let _ = SYNC_HANDLER.set(Box::new(f));
}

/// Perform `kind(payload)` on the host and wait for its answer. `Err` carries the host's
/// message, or says that no suspending bridge is available.
pub fn sync_call(kind: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    if let Some(handler) = SYNC_HANDLER.get() {
        return handler(kind, payload);
    }
    #[cfg(target_arch = "wasm32")]
    {
        wasm::sync_call(kind, payload)
    }
    #[cfg(not(target_arch = "wasm32"))]
    Err("no suspending host bridge is installed".to_string())
}

#[cfg(target_arch = "wasm32")]
pub use wasm::{call_host, describe, set_host};

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::cell::RefCell;

    use js_sys::{Array, Function, Reflect, Uint8Array};
    use wasm_bindgen::{JsCast, JsValue};

    thread_local! {
        static HOST: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    }

    /// Install the JS host object the ops call into.
    pub fn set_host(host: JsValue) {
        HOST.with(|h| *h.borrow_mut() = Some(host));
    }

    /// A readable message for a thrown JS value.
    pub fn describe(v: &JsValue) -> String {
        if let Some(s) = v.as_string() {
            return s;
        }
        if let Ok(m) = Reflect::get(v, &"message".into()) {
            if let Some(s) = m.as_string() {
                return s;
            }
        }
        format!("{v:?}")
    }

    /// Call `host[method](...args)`; `Err` is the message when the host or method is missing or
    /// the call throws.
    pub fn call_host(method: &str, args: &[JsValue]) -> Result<JsValue, String> {
        let host = HOST
            .with(|h| h.borrow().clone())
            .ok_or_else(|| "no browser host installed".to_string())?;
        let f = Reflect::get(&host, &method.into()).map_err(|e| describe(&e))?;
        let f: Function = f
            .dyn_into()
            .map_err(|_| format!("the browser host has no '{method}' method"))?;
        let list = Array::new();
        for a in args {
            list.push(a);
        }
        f.apply(&host, &list).map_err(|e| describe(&e))
    }

    pub fn sync_call(kind: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        let out = call_host("syncCall", &[kind.into(), Uint8Array::from(payload).into()])?;
        let bytes: Uint8Array = out
            .dyn_into()
            .map_err(|_| "syncCall must return a Uint8Array".to_string())?;
        Ok(bytes.to_vec())
    }
}
