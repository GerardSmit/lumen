//! The browser flavour of the network ops (wasm32): `fetch` and `WebSocket` are bridged to the
//! embedding page's own implementations through the host object (see `lumen_host::browser`);
//! HTTP servers and `EventSource` have no browser counterpart and throw.
//!
//! Every request registers a task, asks the host to start it, and returns; the host later pushes
//! a `lumen_host::browser::Event` for that task id and the loop settles it like any other
//! completion.

use js_sys::{Array, Uint8Array};
use lumen_host::browser::{arg_value, call_host, decode_event, Arg, Event};
use lumen_host::{Ctx, Value};
use wasm_bindgen::JsValue;

fn unsupported(ctx: &mut Ctx, what: &str) -> Value {
    let err = ctx.make_error("Error", format!("{what} is not supported in the browser"));
    let _ = ctx.set_member(&err, "code", Value::str("ERR_NOT_SUPPORTED_IN_BROWSER"));
    err
}

fn string_arg(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

fn host_failure(ctx: &mut Ctx, message: String) -> Value {
    ctx.make_error("Error", message)
}

/// `(method, url, headerPairs, bodyOrUndefined, resolve, reject)`.
pub(crate) fn op_http_request(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let method = string_arg(ctx, args, 0)?;
    let target = string_arg(ctx, args, 1)?;
    let headers = crate::read_header_pairs(ctx, args.get(2).unwrap_or(&Value::Undefined))?;
    let body = match args.get(3) {
        None | Some(Value::Undefined) | Some(Value::Null) => None,
        Some(v) => match ctx.typed_array_bytes(v) {
            Some(bytes) => Some(bytes),
            None => Some(ctx.coerce_string(v)?.as_bytes().to_vec()),
        },
    };
    let (resolve, reject) = match (args.get(4), args.get(5)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            (res.clone(), rej.clone())
        }
        _ => return Err(ctx.make_error("TypeError", "__http.request expects (resolve, reject)")),
    };
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
    if let Some(value) = args.get(7).filter(|value| value.as_obj().is_some()) {
        for name in ["mode", "credentials", "redirect"] {
            let option = ctx.get_member(value, name)?;
            if !matches!(option, Value::Undefined) {
                let option = ctx.coerce_string(&option)?.to_string();
                js_sys::Reflect::set(&options, &JsValue::from_str(name), &JsValue::from_str(&option))
                    .map_err(|_| ctx.make_error("TypeError", "invalid browser Fetch options"))?;
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
        return Err(ctx.make_error("TypeError", format!("fetch failed: {message}")));
    }
    Ok(ctx.new_instance(crate::request_control::RequestControl { id }))
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
            Some(Arg::Str(s)) => s,
            _ => "fetch failed".to_string(),
        };
        return Err(ctx.make_error("TypeError", message));
    }
    let mut it = event.args.into_iter().peekable();
    let (
        Some(Arg::Num(status)),
        Some(Arg::Str(status_text)),
        Some(Arg::Str(url)),
        Some(body),
    ) = (it.next(), it.next(), it.next(), it.next())
    else {
        return Err(ctx.make_error("TypeError", "fetch: malformed response from the host"));
    };
    let metadata = if matches!(it.peek(), Some(Arg::Bool(_))) {
        match (it.next(), it.next()) {
            (Some(Arg::Bool(redirected)), Some(Arg::Str(kind))) => Some((redirected, kind)),
            _ => return Err(ctx.make_error("TypeError", "fetch: malformed response metadata")),
        }
    } else { None };
    let mut pairs = Vec::new();
    while let (Some(Arg::Str(k)), Some(Arg::Str(v))) = (it.next(), it.next()) {
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
        Arg::Num(id) if id.is_finite() && id >= 0.0 && id.fract() == 0.0 => {
            let reader = ctx.new_instance(crate::http_body::ResponseBody { request_id: id as u64 });
            let _ = ctx.set_member(&obj, "bodyReader", reader);
        }
        Arg::Null => { let _ = ctx.set_member(&obj, "body", Value::Null); }
        Arg::Bytes(bytes) => {
            let body = arg_value(ctx, Arg::Bytes(bytes))?;
            let _ = ctx.set_member(&obj, "body", body);
        }
        _ => return Err(ctx.make_error("TypeError", "fetch: invalid body handle")),
    }
    Ok(vec![obj])
}

// ---- WebSocket ----

/// `__ws.connect(url, protocols, dispatch)` -> id. The host opens a browser `WebSocket` and
/// pushes `open` / `text` / `binary` / `close` / `error` events for the returned id.
pub(crate) fn op_ws_connect(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = string_arg(ctx, args, 0)?;
    let protocols = string_arg(ctx, args, 1)?;
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "connect: dispatch must be a function")),
    };
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
            protocols.into(),
        ],
    ) {
        if let Some(r) = ctx.host_mut::<lumen_host::TaskRegistry>() {
            r.cancel(id);
        }
        return Err(host_failure(ctx, format!("WebSocket: {message}")));
    }
    Ok(Value::Num(id as f64))
}

/// `__ws.send(id, stringOrBytes)` -> whether the host accepted it.
pub(crate) fn op_ws_send(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(Value::Num(id)) = args.first() else {
        return Err(ctx.make_error("TypeError", "send: bad socket id"));
    };
    let data: JsValue = match args.get(1) {
        Some(v) => match ctx.typed_array_bytes(v) {
            Some(b) => Uint8Array::from(b.as_slice()).into(),
            None => ctx.coerce_string(v)?.to_string().into(),
        },
        None => return Err(ctx.make_error("TypeError", "send: missing data")),
    };
    match call_host("wsSend", &[JsValue::from_f64(*id), data]) {
        Ok(v) => Ok(Value::Bool(v.as_bool().unwrap_or(true))),
        Err(message) => Err(host_failure(ctx, format!("WebSocket send: {message}"))),
    }
}

/// `__ws.close(id, code, reason)`.
pub(crate) fn op_ws_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(Value::Num(id)) = args.first() else {
        return Err(ctx.make_error("TypeError", "close: bad socket id"));
    };
    let code = match args.get(1) {
        Some(Value::Num(n)) => *n,
        _ => 1000.0,
    };
    let reason = string_arg(ctx, args, 2)?;
    let _ = call_host(
        "wsClose",
        &[
            JsValue::from_f64(*id),
            JsValue::from_f64(code),
            reason.into(),
        ],
    );
    Ok(Value::Undefined)
}

pub(crate) fn op_ws_upgrade(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Err(unsupported(ctx, "Accepting WebSocket connections"))
}

// ---- servers and EventSource ----

pub(crate) fn op_server_listen(
    ctx: &mut Ctx,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Err(unsupported(ctx, "Lumen.serve"))
}

pub(crate) fn op_server_respond(
    ctx: &mut Ctx,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Err(unsupported(ctx, "Lumen.serve"))
}

pub(crate) fn op_server_close(
    _ctx: &mut Ctx,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Ok(Value::Undefined)
}

pub(crate) fn op_server_version(
    _ctx: &mut Ctx,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Ok(Value::from_string(env!("CARGO_PKG_VERSION").to_string()))
}

pub(crate) fn op_sse_connect(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Err(unsupported(ctx, "EventSource"))
}

pub(crate) fn op_sse_close(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}
