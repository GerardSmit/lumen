//! The browser flavour of the network ops (wasm32): `fetch` and `WebSocket` are bridged to the
//! embedding page's own implementations through the host object (see `lumen_host::browser`);
//! HTTP servers and `EventSource` have no browser counterpart and throw.
//!
//! Every request registers a task, asks the host to start it, and returns; the host later pushes
//! a `lumen_host::browser::Event` for that task id and the loop settles it like any other
//! completion.

use js_sys::{Array, Uint8Array};
use lumen_bind::NativeError;
use lumen_host::browser::{arg_value, call_host, decode_event, Arg, Event};
use lumen_host::{Ctx, OpError, Value};
use wasm_bindgen::JsValue;

fn unsupported(what: &str) -> NativeError {
    NativeError::runtime(format!("{what} is not supported in the browser")).with_code("ERR_NOT_SUPPORTED_IN_BROWSER")
}

pub(crate) mod http_ops {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__http")]
    pub(crate) mod bindings {
        use super::*;

        /// `(method, url, headerPairs, bodyOrUndefined, resolve, reject)`.
        #[op(coerce)]
        fn request(
            ctx: &mut Ctx,
            method: String,
            target: String,
            headers: Value,
            body: Value,
            resolve: Value,
            reject: Value,
        ) -> Result<(), OpError> {
            let headers = crate::read_header_pairs(ctx, &headers)?;
            let body = crate::read_body(ctx, &body)?;
            let (resolve, reject) = crate::callbacks(resolve, reject, "__http.request")?;
            let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_fetch);
            let pairs = Array::new();
            for (k, v) in &headers {
                let pair = Array::new();
                pair.push(&JsValue::from_str(k));
                pair.push(&JsValue::from_str(v));
                pairs.push(&pair);
            }
            let body = match &body {
                Some(b) => Uint8Array::from(b.as_slice()).into(),
                None => JsValue::NULL,
            };
            let started = call_host(
                "fetch",
                &[JsValue::from_f64(id as f64), method.into(), target.into(), pairs.into(), body],
            );
            if let Err(message) = started {
                if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
                    r.cancel(id);
                }
                return Err(NativeError::type_error(format!("fetch failed: {message}")).into());
            }
            Ok(())
        }
    }

    /// `ok` events carry `[status, statusText, url, body, name1, value1, ...]`, `error` events the
    /// failure message.
    fn decode_fetch(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
        let event = *payload.downcast::<Event>().expect("fetch payload");
        if event.kind != "ok" {
            let message = match event.args.into_iter().next() {
                Some(Arg::Str(s)) => s,
                _ => "fetch failed".to_string(),
            };
            return Err(ctx.make_error("TypeError", message));
        }
        let mut it = event.args.into_iter();
        let (Some(Arg::Num(status)), Some(Arg::Str(status_text)), Some(Arg::Str(url)), Some(Arg::Bytes(body))) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(ctx.make_error("TypeError", "fetch: malformed response from the host"));
        };
        let mut pairs = Vec::new();
        while let (Some(Arg::Str(k)), Some(Arg::Str(v))) = (it.next(), it.next()) {
            pairs.push(ctx.make_array(vec![Value::from_string(k), Value::from_string(v)]));
        }
        let obj = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&obj, "status", Value::Num(status));
        let _ = ctx.set_member(&obj, "statusText", Value::from_string(status_text));
        let _ = ctx.set_member(&obj, "url", Value::from_string(url));
        let headers = ctx.make_array(pairs);
        let _ = ctx.set_member(&obj, "headers", headers);
        let body = arg_value(ctx, Arg::Bytes(body))?;
        let _ = ctx.set_member(&obj, "body", body);
        Ok(vec![obj])
    }
}

pub(crate) mod websocket {
    use super::*;

    #[derive(Default)]
    pub(crate) struct WsRegistry;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__ws")]
    pub(crate) mod bindings {
        use super::*;

        /// `__ws.connect(url, protocols, dispatch)` -> id. The host opens a browser `WebSocket`
        /// and pushes `open` / `text` / `binary` / `close` / `error` events for the returned id.
        #[op(coerce)]
        fn connect(ctx: &mut Ctx, target: String, protocols: String, dispatch: Value) -> Result<f64, NativeError> {
            if !dispatch.is_callable() {
                return Err(NativeError::type_error("connect: dispatch must be a function"));
            }
            let id = {
                let registry = ctx
                    .host_mut::<lumen_host::TaskRegistry>()
                    .expect("runtime task registry");
                registry.register_stream(dispatch, decode_event)
            };
            if let Err(message) = call_host(
                "wsOpen",
                &[JsValue::from_f64(id as f64), target.into(), protocols.into()],
            ) {
                if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
                    r.cancel(id);
                }
                return Err(NativeError::runtime(format!("WebSocket: {message}")));
            }
            Ok(id as f64)
        }

        /// `__ws.send(id, stringOrBytes)` -> whether the host accepted it.
        #[op]
        fn send(ctx: &mut Ctx, id: f64, data: Value) -> Result<bool, OpError> {
            let data: JsValue = match ctx.typed_array_bytes(&data) {
                Some(b) => Uint8Array::from(b.as_slice()).into(),
                None => ctx.coerce_string(&data)?.to_string().into(),
            };
            match call_host("wsSend", &[JsValue::from_f64(id), data]) {
                Ok(v) => Ok(v.as_bool().unwrap_or(true)),
                Err(message) => Err(NativeError::runtime(format!("WebSocket send: {message}")).into()),
            }
        }

        /// `__ws.close(id, code, reason)`.
        #[op(coerce)]
        fn close(id: f64, code: Option<f64>, reason: String) {
            let code = code.unwrap_or(1000.0);
            let _ = call_host("wsClose", &[JsValue::from_f64(id), JsValue::from_f64(code), reason.into()]);
        }

        #[op]
        fn upgrade(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("Accepting WebSocket connections"))
        }
    }
}

pub(crate) mod server {
    use super::*;

    #[derive(Default)]
    pub(crate) struct ServerRegistry;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__http_server")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        fn listen(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("Lumen.serve"))
        }

        #[op]
        fn respond(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("Lumen.serve"))
        }

        #[op]
        fn close(#[varargs] _args: &[Value]) {}

        #[op]
        fn version() -> String {
            env!("CARGO_PKG_VERSION").to_string()
        }
    }
}

pub(crate) mod sse {
    use super::*;

    #[derive(Default)]
    pub(crate) struct SseRegistry;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__sse")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        fn connect(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("EventSource"))
        }

        #[op]
        fn close(#[varargs] _args: &[Value]) {}
    }
}
