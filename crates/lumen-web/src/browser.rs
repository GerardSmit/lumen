//! The browser flavour of the network ops (wasm32): `fetch` and `WebSocket` are bridged to the
//! embedding page's own implementations through the host object (see `lumen_host::browser`);
//! HTTP servers and `EventSource` have no browser counterpart and throw.
//!
//! Every request registers a task, asks the host to start it, and returns; the host later pushes
//! a `lumen_host::browser::Event` for that task id and the loop settles it like any other
//! completion.

use js_sys::{Array, Uint8Array};
use lumen_bind::{Data, NativeError};
use lumen_host::browser::{arg_value, call_host, decode_event, unsupported, Arg, Event};
use lumen_host::{Ctx, OpError, Value};
use wasm_bindgen::JsValue;

pub(crate) mod http_ops {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__http")]
    pub(crate) mod bindings {
        use super::*;

        /// `(method, url, headerPairs, bodyOrUndefined, resolve, reject, _redirectMode, options?)`
        /// -> the request's cancellation control.
        #[op(coerce)]
        #[allow(clippy::too_many_arguments)]
        fn request(
            ctx: &mut Ctx,
            method: String,
            target: String,
            headers: Value,
            body: Value,
            resolve: Value,
            reject: Value,
            _redirect_mode: Option<Value>,
            fetch_options: Option<Value>,
        ) -> Result<Value, OpError> {
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
            let options = js_sys::Object::new();
            if let Some(value) = fetch_options.filter(|value| value.as_obj().is_some()) {
                for name in ["mode", "credentials", "redirect"] {
                    let option = ctx
                        .get_member(&value, name)
                        .map_err(|abrupt| OpError::thrown(lumen::embed::abrupt_value(abrupt)))?;
                    if !matches!(option, Value::Undefined) {
                        let option = ctx.coerce_string(&option)?.to_string();
                        js_sys::Reflect::set(
                            &options,
                            &JsValue::from_str(name),
                            &JsValue::from_str(&option),
                        )
                        .map_err(|_| NativeError::type_error("invalid browser Fetch options"))?;
                    }
                }
                for name in ["uploadProgress", "forcePreflight"] {
                    let option = ctx
                        .get_member(&value, name)
                        .map_err(|abrupt| OpError::thrown(lumen::embed::abrupt_value(abrupt)))?;
                    if let Value::Bool(flag) = option {
                        js_sys::Reflect::set(
                            &options,
                            &JsValue::from_str(name),
                            &JsValue::from_bool(flag),
                        )
                        .map_err(|_| NativeError::type_error("invalid browser Fetch options"))?;
                    }
                }
            }
            let started = call_host(
                "fetch",
                &[
                    JsValue::from_f64(id as f64),
                    method.into(),
                    target.into(),
                    pairs.into(),
                    body,
                    options.into(),
                ],
            );
            if let Err(message) = started {
                if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
                    r.cancel(id);
                }
                return Err(NativeError::type_error(format!("fetch failed: {message}")).into());
            }
            Ok(ctx.new_instance(crate::request_control::RequestControl { id }))
        }
    }

    /// Headers arrive before the body: `[status, statusText, url, requestIdOrNull,
    /// name1, value1, ...]`. Byte payloads remain accepted from older embedders.
    fn decode_fetch(
        ctx: &mut Ctx,
        payload: Box<dyn std::any::Any + Send>,
    ) -> Result<Vec<Value>, Value> {
        let event = *payload.downcast::<Event>().expect("fetch payload");
        if event.kind != "ok" {
            let message = match event.args.into_iter().next() {
                Some(Arg::Data(Data::Str(s))) => s,
                _ => "fetch failed".to_string(),
            };
            return Err(ctx.make_error("TypeError", message));
        }
        let mut it = event.args.into_iter().peekable();
        let (
            Some(Arg::Data(Data::Float(status))),
            Some(Arg::Data(Data::Str(status_text))),
            Some(Arg::Data(Data::Str(url))),
            Some(body),
        ) = (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(ctx.make_error("TypeError", "fetch: malformed response from the host"));
        };
        let metadata = if matches!(it.peek(), Some(Arg::Data(Data::Bool(_)))) {
            match (it.next(), it.next()) {
                (Some(Arg::Data(Data::Bool(redirected))), Some(Arg::Data(Data::Str(kind)))) => {
                    Some((redirected, kind))
                }
                _ => return Err(ctx.make_error("TypeError", "fetch: malformed response metadata")),
            }
        } else {
            None
        };
        let mut pairs = Vec::new();
        while let (Some(Arg::Data(Data::Str(k))), Some(Arg::Data(Data::Str(v)))) =
            (it.next(), it.next())
        {
            pairs.push(ctx.make_array(vec![Value::from_string(k), Value::from_string(v)]));
        }
        let obj = Value::Obj(ctx.new_object());
        if let Some((redirected, kind)) = metadata {
            let _ = ctx.set_member(&obj, "redirected", Value::Bool(redirected));
            let _ = ctx.set_member(&obj, "type", Value::from_string(kind));
        }
        let _ = ctx.set_member(&obj, "status", Value::Num(status));
        let _ = ctx.set_member(&obj, "statusText", Value::from_string(status_text));
        let _ = ctx.set_member(&obj, "url", Value::from_string(url));
        let headers = ctx.make_array(pairs);
        let _ = ctx.set_member(&obj, "headers", headers);
        match body {
            Arg::Data(Data::Float(id)) if id.is_finite() && id >= 0.0 && id.fract() == 0.0 => {
                let reader = ctx.new_instance(crate::http_body::ResponseBody {
                    request_id: id as u64,
                });
                let _ = ctx.set_member(&obj, "bodyReader", reader);
            }
            Arg::Null => {
                let _ = ctx.set_member(&obj, "body", Value::Null);
            }
            body @ Arg::Data(Data::Bytes(_)) => {
                let body = arg_value(ctx, body)?;
                let _ = ctx.set_member(&obj, "body", body);
            }
            _ => return Err(ctx.make_error("TypeError", "fetch: invalid body handle")),
        }
        Ok(vec![obj])
    }
}

pub(crate) mod websocket {
    use super::*;
    use crate::websocket_class::Outgoing;

    /// Open a browser `WebSocket`; the host pushes `open` / `text` / `binary` / `close` / `error`
    /// events for the returned id into `dispatch`.
    pub(crate) fn connect_socket(
        ctx: &mut Ctx,
        target: &str,
        protocols: &[String],
        dispatch: Value,
    ) -> Result<u64, NativeError> {
        if !dispatch.is_callable() {
            return Err(NativeError::type_error(
                "connect: dispatch must be a function",
            ));
        }
        let id = {
            let registry = ctx
                .host_mut::<lumen_host::TaskRegistry>()
                .expect("runtime task registry");
            registry.register_stream(dispatch, decode_event)
        };
        if let Err(message) = call_host(
            "wsOpen",
            &[
                JsValue::from_f64(id as f64),
                target.into(),
                protocols.join(", ").into(),
            ],
        ) {
            if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
                r.cancel(id);
            }
            return Err(NativeError::runtime(format!("WebSocket: {message}")));
        }
        Ok(id)
    }

    /// Whether the host accepted the message.
    pub(crate) fn send_frame(
        _ctx: &mut Ctx,
        id: u64,
        payload: Outgoing<'_>,
    ) -> Result<bool, NativeError> {
        let data: JsValue = match payload {
            Outgoing::Binary(bytes) => Uint8Array::from(bytes).into(),
            Outgoing::Text(text) => text.into(),
        };
        match call_host("wsSend", &[JsValue::from_f64(id as f64), data]) {
            Ok(v) => Ok(v.as_bool().unwrap_or(true)),
            Err(message) => Err(NativeError::runtime(format!("WebSocket send: {message}"))),
        }
    }

    pub(crate) fn close_socket(_ctx: &mut Ctx, id: u64, code: u16, reason: &str) {
        let _ = call_host(
            "wsClose",
            &[
                JsValue::from_f64(id as f64),
                JsValue::from_f64(code as f64),
                reason.into(),
            ],
        );
    }

    /// Close a socket nobody listens to any more and stop delivering its events.
    pub(crate) fn abandon_socket(ctx: &mut Ctx, id: u64) {
        close_socket(ctx, id, 1001, "");
        if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
            r.cancel(id);
        }
    }
}

pub(crate) mod server {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "Lumen")]
    pub(crate) mod bindings {
        use super::*;

        #[op(hint(js(webidl)))]
        fn serve(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("Lumen.serve"))
        }

        #[op(name = "upgradeWebSocket", hint(js(webidl)))]
        fn upgrade_websocket(#[varargs] _args: &[Value]) -> Result<(), NativeError> {
            Err(unsupported("Lumen.upgradeWebSocket"))
        }

        #[init]
        fn init(ctx: &mut Ctx, target: &Value) -> Result<(), Value> {
            crate::wasm_ops::define_data(
                ctx,
                target,
                "version",
                Value::str(env!("CARGO_PKG_VERSION")),
                false,
                true,
                true,
            )
        }
    }
}

pub(crate) mod sse {
    use super::*;

    pub(crate) fn connect_stream(
        _ctx: &mut Ctx,
        _target: &str,
        _last_event_id: &str,
        _dispatch: Value,
    ) -> Result<u64, NativeError> {
        Err(unsupported("EventSource"))
    }

    pub(crate) fn close_stream(_ctx: &mut Ctx, _id: u64) {}
}
