//! A blocking HTTP/1.1 *server* on `std::net::TcpListener`, the mirror of the fetch client in
//! `http.rs`. `Lumen.serve(handler)` drives a WinterCG-style fetch
//! handler: each connection is parsed into a `Request`, the handler returns a `Response`, and
//! the bytes are written back — the same `(Request) -> Response` contract Deno.serve/Bun.serve
//! and Cloudflare Workers converge on (WinterTC's Minimum Common API standardizes
//! fetch/Request/Response but not a server, so this follows that cross-runtime convention).
//!
//! `Lumen.serve`, `Lumen.upgradeWebSocket` and `Lumen.version` are one `lumen_bind` module
//! (`bindings`). Requests are native `Request` objects and responses are read from native
//! `Response` objects (`lumen_host::net`), so nothing round-trips through script glue.
//!
//! ## How it runs on the loop
//! The engine is single-threaded and `!Send`, so the JS handler must run on the loop thread.
//! We reuse the runtime's only async primitive — `spawn_blocking` + a `TaskRegistry`
//! completion — with a **re-arm** pattern: one accept task runs on a pool thread, blocks in
//! `accept()`, reads+parses one request, and comes back to the loop as a completion. The
//! completion decoder ([`decode_accept`]) hands the request to the handler *and* arms the next accept,
//! so the listener keeps running for the life of the process (an always-registered task is
//! also what keeps the event loop from going idle). Responses are written back on the pool
//! (blocking `write`), settled through the registry like any other async op.
//!
//! ## What's intentionally missing (v1 — cold-start / low-concurrency focus)
//! - **Concurrency**: exactly one `accept()` is in flight at a time, and it holds one of the
//!   pool's worker threads while blocked. Fine for cold-start latency and light load; a real
//!   readiness reactor (epoll/kqueue) or a dedicated listener thread is future work and would
//!   need raw syscalls (out of scope under the zero-dep policy) or a new host primitive.
//! - **Keep-alive**: every response is `Connection: close`; one request per connection.
//! - **Streaming**: request and response bodies are fully buffered (no chunked *response*
//!   output, no backpressure) — same limitation the fetch client / body streams have today.
//! - **No HTTP/2, no `Expect: 100-continue`, no trailers on responses, no `Date` header**
//!   (formatting an HTTP-date without a date library is deferred), **no TLS/https** (same
//!   STOP-AND-FLAG as the client: TLS can't be built on std alone).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lumen::embed::{Deferred, OpResult};
use lumen_bind::NativeError;
use lumen_host::net::{
    headers_entries, read_served_body, request_header, server_request, served_response,
    ServedResponse,
};
use lumen_host::{Ctx, OpError, SpawnHandle, Value};

use crate::http::{read_body_exact, read_capped_line, read_chunked};
use crate::websocket::{adopt_connection, close_socket, send_frame};
use crate::websocket_class::Outgoing;

/// A slow or idle client must not pin a pool worker forever: bound the header/body read.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Request-line + headers size cap (a hostile client can't balloon the worker).
const MAX_HEADER_BYTES: usize = crate::http::MAX_HEADER_BYTES;

/// Live servers and the connections awaiting an answer. Lives in `OpState`; the accept decoder
/// reads it to re-arm and `shutdown` flips the `closed` flag. Script values (the handlers) are
/// `!Send` and so only ever touched on the loop thread, never moved into a pool closure.
#[derive(Default)]
pub(crate) struct ServerRegistry {
    next: u64,
    servers: HashMap<u64, ServerEntry>,
    conns: HashMap<u32, Conn>,
    /// The hidden slot on a delivered `Request` that holds its connection id.
    conn_slot: Option<String>,
    /// Receives the completions whose work is done by their decoder.
    noop: Option<Value>,
}

struct ServerEntry {
    listener: Arc<TcpListener>,
    local_addr: SocketAddr,
    /// Flipped by `shutdown`; the accept loop checks it and stops re-arming.
    closed: Arc<AtomicBool>,
    handler: Rc<Handler>,
    /// Resolved when the listener has stopped.
    finished: Option<Deferred>,
}

/// What a server calls: `call.call(this, request, info)`, and the `onError` hook.
struct Handler {
    call: Value,
    this: Value,
    on_error: Option<Value>,
}

/// A connection delivered to a handler that has not been answered yet.
struct Conn {
    remote: String,
    upgraded: bool,
    /// Answers to `HEAD` carry the headers of the `GET` answer and no body.
    head_request: bool,
}

/// What one accept task sends back to the loop (all fields `Send`).
struct AcceptTaskResult {
    server_id: u64,
    outcome: AcceptOutcome,
}

enum AcceptOutcome {
    /// A parsed request plus the still-open socket to answer on.
    Request(Accepted),
    /// The listener was shut down (or `accept()` failed): stop serving this server.
    Closed,
}

struct Accepted {
    stream: TcpStream,
    peer: SocketAddr,
    method: String,
    /// Absolute URL (`http://<host><target>`) so the `Request` constructor accepts it.
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn registry(ctx: &mut Ctx) -> &mut ServerRegistry {
    ctx.host_mut::<ServerRegistry>()
        .expect("web installs ServerRegistry")
}

fn noop(ctx: &mut Ctx) -> Value {
    if let Some(noop) = registry(ctx).noop.clone() {
        return noop;
    }
    let noop = ctx.new_native_fn(
        "",
        0,
        Rc::new(|_: &mut Ctx, _: Value, _: &[Value]| Ok(Value::Undefined)),
    );
    registry(ctx).noop = Some(noop.clone());
    noop
}

fn conn_slot(ctx: &mut Ctx) -> String {
    if let Some(slot) = registry(ctx).conn_slot.clone() {
        return slot;
    }
    let slot = ctx.allocate_native_private_slot_name();
    registry(ctx).conn_slot = Some(slot.clone());
    slot
}

// ---- Lumen.serve --------------------------------------------------------------------------------

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "Lumen")]
mod bindings {
    use super::*;

    /// `Lumen.serve(handler[, options])`, `Lumen.serve(options, handler)` or
    /// `Lumen.serve({ fetch, ...options })`: an HTTP server whose handler is
    /// `(request, info) => Response | Promise<Response>`.
    #[op(hint(js(webidl)))]
    fn serve(ctx: &mut Ctx, a: Value, #[default(Value::Undefined)] b: Value) -> OpResult<Value> {
        super::serve(ctx, a, b)
    }

    /// `Lumen.upgradeWebSocket(request[, { protocol, headers }])`: hands a `Lumen.serve`
    /// connection over to the WebSocket machinery. `null` when the request is not a WebSocket
    /// upgrade, else `{ send(data) -> bool, close(code?, reason?), remoteAddress, onmessage,
    /// onclose }`.
    #[op(name = "upgradeWebSocket", hint(js(webidl)))]
    fn upgrade_websocket(
        ctx: &mut Ctx,
        request: Value,
        #[default(Value::Undefined)] options: Value,
    ) -> OpResult<Value> {
        super::upgrade_websocket(ctx, request, options)
    }

    /// Defines `Lumen.version`, the runtime version string.
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

fn type_error(message: &'static str) -> OpError {
    OpError::type_error(message)
}

fn serve_arguments() -> OpError {
    type_error("Lumen.serve requires a handler function or an object with a fetch() method")
}

fn thrown(value: Value) -> OpError {
    OpError::thrown(value)
}

fn option(ctx: &mut Ctx, options: &Value, name: &str) -> OpResult<Value> {
    match options {
        Value::Obj(_) => ctx.member_get(options, name).map_err(thrown),
        _ => Ok(Value::Undefined),
    }
}

fn is_plain_object(value: &Value) -> bool {
    matches!(value, Value::Obj(_)) && !value.is_callable()
}

fn serve(ctx: &mut Ctx, a: Value, b: Value) -> OpResult<Value> {
    let (call, this, options, from_object) = if a.is_callable() {
        let options = if is_plain_object(&b) { b } else { Value::Undefined };
        (a, Value::Undefined, options, false)
    } else if is_plain_object(&a) && b.is_callable() {
        (b, Value::Undefined, a, false)
    } else if is_plain_object(&a) {
        let fetch = ctx.member_get(&a, "fetch").map_err(thrown)?;
        if !fetch.is_callable() {
            return Err(serve_arguments());
        }
        (fetch, a.clone(), a, true)
    } else {
        return Err(serve_arguments());
    };

    let hostname = match option(ctx, &options, "hostname")? {
        Value::Str(text) => text.to_string(),
        _ => "0.0.0.0".to_owned(),
    };
    let port = match option(ctx, &options, "port")? {
        Value::Num(port) if (0.0..=65535.0).contains(&port) && port.fract() == 0.0 => port as u16,
        Value::Num(_) => return Err(OpError::new("RangeError", "serve: port must be 0..=65535")),
        _ => 8000,
    };
    let on_listen = Some(option(ctx, &options, "onListen")?).filter(Value::is_callable);
    // An object's own `onError` is skipped when the handler came off the object: a framework
    // method (Hono's `app.onError`) must not be mistaken for a serve option.
    let on_error = if from_object {
        None
    } else {
        Some(option(ctx, &options, "onError")?).filter(Value::is_callable)
    };
    let signal = option(ctx, &options, "signal")?;

    let listener = TcpListener::bind((hostname.as_str(), port))
        .map_err(|e| NativeError::runtime(format!("listen {hostname}:{port}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| NativeError::runtime(format!("local_addr: {e}")))?;
    let listener = Arc::new(listener);
    let closed = Arc::new(AtomicBool::new(false));
    let handler = Rc::new(Handler { call, this, on_error });
    let finished = Deferred::new(ctx);
    let promise = finished.promise();

    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.servers.insert(
            id,
            ServerEntry {
                listener: Arc::clone(&listener),
                local_addr,
                closed: Arc::clone(&closed),
                handler: handler.clone(),
                finished: Some(finished),
            },
        );
        id
    };
    arm_accept(ctx, id, listener, closed, local_addr);

    let shutdown = {
        let promise = promise.clone();
        ctx.new_native_fn(
            "shutdown",
            0,
            Rc::new(move |ctx: &mut Ctx, _: Value, _: &[Value]| {
                close_server(ctx, id);
                Ok(promise.clone())
            }),
        )
    };

    if matches!(signal, Value::Obj(_)) {
        let add = ctx.member_get(&signal, "addEventListener").map_err(thrown)?;
        if add.is_callable() {
            let aborted = ctx.member_get(&signal, "aborted").map_err(thrown)?;
            if ctx.to_boolean(&aborted) {
                ctx.invoke(shutdown.clone(), Value::Undefined, &[]).map_err(thrown)?;
            } else {
                let once = ctx.plain_object(&[("once", Value::Bool(true))]);
                ctx.invoke(add, signal, &[Value::str("abort"), shutdown.clone(), once])
                    .map_err(thrown)?;
            }
        }
    }

    let bound_port = Value::Num(local_addr.port() as f64);
    if let Some(on_listen) = on_listen {
        let info = ctx.plain_object(&[
            ("hostname", Value::from_string(hostname.clone())),
            ("port", bound_port.clone()),
        ]);
        ctx.invoke(on_listen, Value::Undefined, &[info]).map_err(thrown)?;
    }

    let reference = noop_method(ctx, "ref");
    let unreference = noop_method(ctx, "unref");
    Ok(ctx.plain_object(&[
        ("hostname", Value::from_string(hostname)),
        ("port", bound_port),
        ("finished", promise),
        ("shutdown", shutdown),
        ("ref", reference),
        ("unref", unreference),
    ]))
}

fn noop_method(ctx: &mut Ctx, name: &str) -> Value {
    ctx.new_native_fn(
        name,
        0,
        Rc::new(|_: &mut Ctx, _: Value, _: &[Value]| Ok(Value::Undefined)),
    )
}

/// Flag the listener closed and poke it with a throwaway connection so the blocked `accept()`
/// wakes, sees the flag, and stops re-arming.
fn close_server(ctx: &mut Ctx, id: u64) {
    let target = registry(ctx)
        .servers
        .get(&id)
        .map(|e| (Arc::clone(&e.closed), e.local_addr));
    if let Some((closed, local_addr)) = target {
        closed.store(true, Ordering::SeqCst);
        wake_accept(local_addr);
    }
}

/// Close every listener the realm still holds, waking their blocked `accept`s.
pub(crate) fn close_all(ctx: &mut Ctx) {
    let servers: Vec<ServerEntry> = ctx
        .host_mut::<ServerRegistry>()
        .map(|reg| reg.servers.drain().map(|(_, e)| e).collect())
        .unwrap_or_default();
    for entry in servers {
        entry.closed.store(true, Ordering::SeqCst);
        wake_accept(entry.local_addr);
    }
}

/// Poke a listener with a throwaway connection so its blocked `accept()` returns. Bounded: a
/// listener that has stopped accepting must not stall the caller.
fn wake_accept(local_addr: SocketAddr) {
    // Connect to a concrete loopback address when bound to the wildcard, so the wake actually
    // reaches our listener.
    let wake_addr = if local_addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_addr.port())
    } else {
        local_addr
    };
    let _ = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(250));
}

// ---- completion decoders (run on the loop thread with &mut Ctx) --------------------------------

/// Settle one accept: on a request, stash the socket, re-arm the next accept and run the
/// handler. On close, resolve the server's `finished` promise and do not re-arm.
fn decode_accept(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let AcceptTaskResult { server_id, outcome } = *payload
        .downcast::<AcceptTaskResult>()
        .expect("accept payload");

    let accepted = match outcome {
        AcceptOutcome::Closed => {
            let finished = registry(ctx)
                .servers
                .remove(&server_id)
                .and_then(|entry| entry.finished);
            if let Some(finished) = finished {
                finished.resolve(ctx, Value::Undefined);
            }
            return Ok(Vec::new());
        }
        AcceptOutcome::Request(accepted) => accepted,
    };

    // Re-arm the next accept from the still-live server entry (a concurrent close removes the
    // entry, in which case this connection is dropped).
    let rearm = registry(ctx).servers.get(&server_id).map(|e| {
        (
            Arc::clone(&e.listener),
            Arc::clone(&e.closed),
            e.local_addr,
            e.handler.clone(),
        )
    });
    let Some((listener, closed, local_addr, handler)) = rearm else {
        return Ok(Vec::new());
    };
    arm_accept(ctx, server_id, listener, closed, local_addr);

    let peer = accepted.peer;
    let conn_id = ctx.resource_table().add(accepted.stream);
    registry(ctx).conns.insert(
        conn_id,
        Conn {
            remote: peer.ip().to_string(),
            upgraded: false,
            head_request: accepted.method.eq_ignore_ascii_case("HEAD"),
        },
    );
    deliver(
        ctx,
        &handler,
        conn_id,
        peer,
        &accepted.method,
        &accepted.url,
        &accepted.headers,
        accepted.body,
    );
    Ok(Vec::new())
}

/// Settle a response write. A failed write means the client hung up, which is not actionable.
fn decode_write(
    _ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let _ = payload
        .downcast::<Result<(), String>>()
        .expect("write payload");
    Ok(Vec::new())
}

// ---- request handling -------------------------------------------------------------------------

/// Build the `Request` and `info`, run the handler, and answer from its result.
#[allow(clippy::too_many_arguments)]
fn deliver(
    ctx: &mut Ctx,
    handler: &Rc<Handler>,
    conn_id: u32,
    peer: SocketAddr,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) {
    let outcome = (|| -> Result<Value, Value> {
        let request = server_request(ctx, method, url, headers, body).map_err(|e| e.to_value(ctx))?;
        let slot = conn_slot(ctx);
        ctx.define_native_private_value_slot(&request, &slot, Value::Num(conn_id as f64))?;
        let remote = ctx.plain_object(&[
            ("transport", Value::str("tcp")),
            ("hostname", Value::from_string(peer.ip().to_string())),
            ("port", Value::Num(peer.port() as f64)),
        ]);
        let info = ctx.plain_object(&[("remoteAddr", remote)]);
        ctx.invoke(handler.call.clone(), handler.this.clone(), &[request, info])
    })();
    match outcome {
        Ok(result) => await_value(ctx, result, handler, conn_id, after_handler),
        Err(error) => fail(ctx, handler, conn_id, error),
    }
}

type Continue = fn(&mut Ctx, &Rc<Handler>, u32, Result<Value, Value>);

/// Continue with the settled `value` (awaited like `await value`).
fn await_value(ctx: &mut Ctx, value: Value, handler: &Rc<Handler>, conn_id: u32, next: Continue) {
    let reaction = |ctx: &mut Ctx, handler: &Rc<Handler>, fulfilled: bool| {
        let handler = handler.clone();
        ctx.new_native_fn(
            "",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                let settled = args.first().cloned().unwrap_or(Value::Undefined);
                next(ctx, &handler, conn_id, if fulfilled { Ok(settled) } else { Err(settled) });
                Ok(Value::Undefined)
            }),
        )
    };
    let on_ok = reaction(ctx, handler, true);
    let on_err = reaction(ctx, handler, false);
    ctx.then_value(value, on_ok, on_err);
}

fn after_handler(ctx: &mut Ctx, handler: &Rc<Handler>, conn_id: u32, result: Result<Value, Value>) {
    match result {
        Ok(response) => answer(ctx, handler, conn_id, response, true),
        Err(error) => fail(ctx, handler, conn_id, error),
    }
}

fn after_error_hook(ctx: &mut Ctx, handler: &Rc<Handler>, conn_id: u32, result: Result<Value, Value>) {
    match result {
        Ok(response) => answer(ctx, handler, conn_id, response, false),
        Err(_) => internal_error(ctx, conn_id),
    }
}

fn upgraded(ctx: &mut Ctx, conn_id: u32) -> bool {
    registry(ctx).conns.get(&conn_id).is_some_and(|conn| conn.upgraded)
}

/// Write the response `response` the handler (or its error hook) produced. A value that is not a
/// `Response` is a handler error, unless the error hook produced it.
fn answer(ctx: &mut Ctx, handler: &Rc<Handler>, conn_id: u32, response: Value, from_handler: bool) {
    // An upgraded connection's socket belongs to the WebSocket machinery; the 101 already went out.
    if upgraded(ctx, conn_id) {
        registry(ctx).conns.remove(&conn_id);
        return;
    }
    let head = served_response(ctx, &response).filter(|head| (100..=599).contains(&head.status));
    let Some(head) = head else {
        if from_handler {
            let error = ctx.make_error("TypeError", "serve handler did not return a Response");
            return fail(ctx, handler, conn_id, error);
        }
        return internal_error(ctx, conn_id);
    };
    read_served_body(
        ctx,
        &response,
        Box::new(move |ctx, body| write_response(ctx, conn_id, head, body)),
    );
}

/// The handler threw (or returned something else than a `Response`): ask the error hook, else log
/// and answer 500.
fn fail(ctx: &mut Ctx, handler: &Rc<Handler>, conn_id: u32, error: Value) {
    if upgraded(ctx, conn_id) {
        registry(ctx).conns.remove(&conn_id);
        return;
    }
    let Some(hook) = handler.on_error.clone() else {
        log_error(ctx, error);
        return internal_error(ctx, conn_id);
    };
    match ctx.invoke(hook, Value::Undefined, &[error]) {
        Ok(result) => await_value(ctx, result, handler, conn_id, after_error_hook),
        Err(_) => internal_error(ctx, conn_id),
    }
}

fn log_error(ctx: &mut Ctx, error: Value) {
    let global = ctx.global_object();
    if let Ok(console) = ctx.member_get(&global, "console") {
        if let Ok(log) = ctx.member_get(&console, "error") {
            let _ = ctx.invoke(log, console, &[error]);
        }
    }
}

fn internal_error(ctx: &mut Ctx, conn_id: u32) {
    let head = ServedResponse {
        status: 500,
        status_text: String::new(),
        headers: vec![("content-type".into(), "text/plain;charset=UTF-8".into())],
    };
    write_response(ctx, conn_id, head, b"Internal Server Error".to_vec());
}

/// Serialize the response and write it on the pool, then close the socket. The socket is taken out
/// of the resource table here, so a connection already answered or upgraded is left alone.
fn write_response(ctx: &mut Ctx, conn_id: u32, head: ServedResponse, body: Vec<u8>) {
    let head_request = registry(ctx)
        .conns
        .remove(&conn_id)
        .is_some_and(|conn| conn.head_request);
    let stream = ctx
        .resource_table()
        .close(conn_id)
        .and_then(|rc| rc.downcast::<TcpStream>().ok())
        .and_then(|rc| Rc::try_unwrap(rc).ok());
    let Some(stream) = stream else {
        return;
    };
    let bytes = build_response(head.status, &head.status_text, &head.headers, &body, head_request);
    let done = noop(ctx);
    let id = lumen_host::register_task(ctx, done.clone(), Some(done), decode_write);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(id, move || Box::new(write_all_and_close(stream, &bytes)));
}

// ---- Lumen.upgradeWebSocket ---------------------------------------------------------------------

fn upgrade_websocket(ctx: &mut Ctx, request: Value, options: Value) -> OpResult<Value> {
    let conn_id = registry(ctx)
        .conn_slot
        .clone()
        .and_then(|slot| ctx.native_private_value_slot(&request, &slot))
        .and_then(|id| match id {
            Value::Num(id) => Some(id as u32),
            _ => None,
        })
        .ok_or_else(|| type_error("upgradeWebSocket: the request was not delivered by Lumen.serve"))?;
    let (remote, upgraded) = match registry(ctx).conns.get(&conn_id) {
        Some(conn) => (conn.remote.clone(), conn.upgraded),
        None => return Err(type_error("upgrade: unknown or already-answered connection")),
    };
    if upgraded {
        return Err(OpError::new("Error", "upgradeWebSocket: connection already upgraded"));
    }
    let wants_upgrade = request_header(ctx, &request, "upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let key = request_header(ctx, &request, "sec-websocket-key").filter(|key| !key.is_empty());
    let (true, Some(key)) = (wants_upgrade, key) else {
        return Ok(Value::Null);
    };

    let protocol = match option(ctx, &options, "protocol")? {
        value if ctx.to_boolean(&value) => ctx.coerce_string(&value).map_err(thrown)?.to_string(),
        _ => String::new(),
    };
    let extra = option(ctx, &options, "headers")?;
    let extra = extra_headers(ctx, &extra)?;

    let handle = ctx.plain_object(&[
        ("remoteAddress", Value::from_string(remote)),
        ("onmessage", Value::Null),
        ("onclose", Value::Null),
    ]);
    let closed = Rc::new(std::cell::Cell::new(false));
    let dispatch = {
        let handle = handle.clone();
        let closed = closed.clone();
        ctx.new_native_fn(
            "",
            3,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                socket_event(ctx, &handle, &closed, args)
            }),
        )
    };
    let id = adopt_connection(ctx, conn_id, &key, &protocol, &extra, dispatch)?;
    if let Some(conn) = registry(ctx).conns.get_mut(&conn_id) {
        conn.upgraded = true;
    }

    let send = {
        let closed = closed.clone();
        ctx.new_native_fn(
            "send",
            1,
            Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                if closed.get() {
                    return Ok(Value::Bool(false));
                }
                let data = args.first().cloned().unwrap_or(Value::Undefined);
                let sent = match ctx.typed_array_bytes(&data) {
                    Some(bytes) => send_frame(ctx, id, Outgoing::Binary(&bytes)),
                    None => {
                        let text = ctx.coerce_string(&data)?;
                        send_frame(ctx, id, Outgoing::Text(&text))
                    }
                };
                match sent {
                    Ok(sent) => Ok(Value::Bool(sent)),
                    Err(error) => Err(OpError::from(error).to_value(ctx)),
                }
            }),
        )
    };
    let close = ctx.new_native_fn(
        "close",
        2,
        Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
            let code = match args.first() {
                None | Some(Value::Undefined) => 1000,
                Some(code) => ctx.coerce_number(code)? as u16,
            };
            let reason = match args.get(1) {
                None | Some(Value::Undefined) => String::new(),
                Some(reason) => ctx.coerce_string(reason)?.to_string(),
            };
            close_socket(ctx, id, code, &reason);
            Ok(Value::Undefined)
        }),
    );
    ctx.member_set(&handle, "send", send).map_err(thrown)?;
    ctx.member_set(&handle, "close", close).map_err(thrown)?;
    Ok(handle)
}

/// One report from a server-side socket's transport: message events call `onmessage(data,
/// isBinary)`; every way the connection ends calls `onclose(code, reason, wasClean)` once.
fn socket_event(
    ctx: &mut Ctx,
    handle: &Value,
    closed: &std::cell::Cell<bool>,
    args: &[Value],
) -> Result<Value, Value> {
    let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Undefined);
    let kind = match args.first() {
        Some(Value::Str(kind)) => kind.to_string(),
        _ => String::new(),
    };
    match kind.as_str() {
        "text" | "binary" => {
            let callback = ctx.member_get(handle, "onmessage")?;
            if callback.is_callable() {
                ctx.invoke(callback, handle.clone(), &[arg(1), Value::Bool(kind == "binary")])?;
            }
        }
        "close" => fire_close(ctx, handle, closed, arg(1), arg(2), true)?,
        "fail" => {
            let reason = Value::from_string(ctx.coerce_string(&arg(2))?.to_string());
            fire_close(ctx, handle, closed, arg(1), reason, false)?
        }
        "io" => {
            let reason = Value::from_string(ctx.coerce_string(&arg(1))?.to_string());
            fire_close(ctx, handle, closed, Value::Num(1006.0), reason, false)?
        }
        _ => {}
    }
    Ok(Value::Undefined)
}

fn fire_close(
    ctx: &mut Ctx,
    handle: &Value,
    closed: &std::cell::Cell<bool>,
    code: Value,
    reason: Value,
    clean: bool,
) -> Result<(), Value> {
    if closed.replace(true) {
        return Ok(());
    }
    let callback = ctx.member_get(handle, "onclose")?;
    if callback.is_callable() {
        ctx.invoke(callback, handle.clone(), &[code, reason, Value::Bool(clean)])?;
    }
    Ok(())
}

/// The extra handshake headers of `upgradeWebSocket`'s options: a `Headers`, an array of
/// `[name, value]` pairs or a plain object.
fn extra_headers(ctx: &mut Ctx, value: &Value) -> OpResult<Vec<(String, String)>> {
    if !matches!(value, Value::Obj(_)) {
        return Ok(Vec::new());
    }
    if let Some(entries) = headers_entries(ctx, value) {
        return Ok(entries);
    }
    let mut pairs = Vec::new();
    if ctx.is_array_value(value).map_err(thrown)? {
        for pair in ctx.iterable_to_list(value, usize::MAX)? {
            let name = ctx.member_get(&pair, "0").map_err(thrown)?;
            let value = ctx.member_get(&pair, "1").map_err(thrown)?;
            pairs.push((
                ctx.coerce_string(&name).map_err(thrown)?.to_string(),
                ctx.coerce_string(&value).map_err(thrown)?.to_string(),
            ));
        }
        return Ok(pairs);
    }
    for key in ctx.reflect_own_keys(value).map_err(thrown)? {
        let Value::Str(name) = &key else { continue };
        let descriptor = ctx.reflect_get_own_property_descriptor(value, &key).map_err(thrown)?;
        if !matches!(descriptor, Value::Obj(_))
            || !matches!(ctx.member_get(&descriptor, "enumerable"), Ok(Value::Bool(true)))
        {
            continue;
        }
        let entry = ctx.member_get(value, name).map_err(thrown)?;
        pairs.push((name.to_string(), ctx.coerce_string(&entry).map_err(thrown)?.to_string()));
    }
    Ok(pairs)
}

// ---- helpers ----------------------------------------------------------------------------------

/// Register a fresh accept task and spawn it on the pool.
fn arm_accept(
    ctx: &mut Ctx,
    server_id: u64,
    listener: Arc<TcpListener>,
    closed: Arc<AtomicBool>,
    local_addr: SocketAddr,
) {
    let done = noop(ctx);
    let id = lumen_host::register_task(ctx, done, None, decode_accept);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    let fallback_host = local_addr.to_string();
    spawn.spawn_blocking(id, move || {
        Box::new(AcceptTaskResult {
            server_id,
            outcome: accept_one(&listener, &closed, &fallback_host),
        })
    });
}

/// Accept and parse exactly one good request (answering malformed ones with 400 inline and
/// moving on), or report the listener as closed.
fn accept_one(listener: &TcpListener, closed: &AtomicBool, fallback_host: &str) -> AcceptOutcome {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => return AcceptOutcome::Closed,
        };
        if closed.load(Ordering::SeqCst) {
            return AcceptOutcome::Closed; // woken by close()'s throwaway connection
        }
        stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
        match read_request(&stream, fallback_host) {
            Ok((method, url, headers, body)) => {
                return AcceptOutcome::Request(Accepted {
                    stream,
                    peer,
                    method,
                    url,
                    headers,
                    body,
                });
            }
            Err(msg) => {
                let (code, text) = if msg.contains("headers too large") {
                    (431, "Request Header Fields Too Large")
                } else {
                    (400, "Bad Request")
                };
                let _ = write_simple(&stream, code, text, msg.as_bytes());
                // Keep serving: fall through to accept the next connection.
            }
        }
    }
}

/// Parse a request off the socket: request line, headers, and a Content-Length/chunked body.
/// Returns `(METHOD, absolute-url, headers, body)`.
#[allow(clippy::type_complexity)]
fn read_request(
    stream: &TcpStream,
    fallback_host: &str,
) -> Result<(String, String, Vec<(String, String)>, Vec<u8>), String> {
    let mut reader = BufReader::new(stream);

    let mut header_budget = MAX_HEADER_BYTES;
    let request_line = read_capped_line(&mut reader, &mut header_budget)
        .map_err(|e| format!("read request line: {e}"))?;
    if request_line.is_empty() {
        return Err("empty request".to_string());
    }
    let mut parts = request_line.trim_end().split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err("malformed request line".to_string());
    }

    let mut headers = Vec::new();
    loop {
        let line = read_capped_line(&mut reader, &mut header_budget)
            .map_err(|e| format!("read headers: {e}"))?;
        if line.is_empty() {
            break; // EOF before the blank line: tolerate it
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(i) = line.find(':') {
            headers.push((line[..i].to_string(), line[i + 1..].trim().to_string()));
        }
    }

    let host = header(&headers, "host").unwrap_or_else(|| fallback_host.to_string());
    let url = if target.starts_with("http://") || target.starts_with("https://") {
        target // absolute-form (proxy requests)
    } else if target.starts_with('/') {
        format!("http://{host}{target}")
    } else {
        // authority-form (CONNECT) or asterisk-form: not meaningfully routable; give a URL the
        // Request constructor still accepts.
        format!("http://{host}/{target}")
    };

    let body = read_request_body(&mut reader, &headers, &method)?;
    Ok((method.to_ascii_uppercase(), url, headers, body))
}

fn read_request_body(
    reader: &mut impl BufRead,
    headers: &[(String, String)],
    method: &str,
) -> Result<Vec<u8>, String> {
    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        return Ok(Vec::new());
    }
    if header(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader).map_err(|e| format!("chunked body: {e}"));
    }
    match header(headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
        Some(len) => read_body_exact(reader, len).map_err(|e| format!("read body: {e}")),
        // No framing on a request means no body (unlike a response, there is no read-to-EOF).
        None => Ok(Vec::new()),
    }
}

/// Serialize a response. We own `Connection`/`Transfer-Encoding` and add `Content-Length` and a
/// `Server` header when the handler didn't set them; everything else the handler chose passes
/// through verbatim.
fn build_response(
    status: u16,
    status_text: &str,
    headers: &[(String, String)],
    body: &[u8],
    head_request: bool,
) -> Vec<u8> {
    let no_content = matches!(status, 100..=199 | 204 | 304);
    let reason = if status_text.is_empty() {
        reason_phrase(status)
    } else {
        status_text
    };
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());

    let (mut has_content_length, mut has_server) = (false, false);
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("transfer-encoding") {
            continue; // we own connection framing
        }
        has_content_length |= k.eq_ignore_ascii_case("content-length");
        has_server |= k.eq_ignore_ascii_case("server");
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_content_length && !no_content {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    if !has_server {
        out.extend_from_slice(
            concat!("Server: lumen/", env!("CARGO_PKG_VERSION"), "\r\n").as_bytes(),
        );
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    if !head_request && !no_content {
        out.extend_from_slice(body);
    }
    out
}

/// A bare status-only response used for the inline 400 path.
fn write_simple(stream: &TcpStream, status: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    let mut s = stream;
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn write_all_and_close(stream: TcpStream, bytes: &[u8]) -> Result<(), String> {
    let mut s = &stream;
    s.write_all(bytes)
        .map_err(|e| format!("response write: {e}"))?;
    s.flush().ok();
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// The default reason phrase for the common statuses; anything else gets an empty phrase (valid
/// per HTTP — clients ignore it).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}
