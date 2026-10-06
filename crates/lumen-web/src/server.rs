//! An HTTP/1.1 *server* on a nonblocking `std::net::TcpListener`, the mirror of the fetch client in
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
//! The engine is single-threaded and `!Send`, so the JS handler runs on the loop thread, and so
//! does all the socket work: the listener and every accepted connection are registrations on the
//! loop's readiness reactor (`nbio`). The reactor wakes the loop when the listener has
//! connections to accept; [`decode_accept`] accepts until the socket would block and gives each
//! connection a parser that is fed whenever the connection is readable. A complete request goes
//! to the handler; the response is written straight to the socket and what the socket does not
//! take is queued and sent when it is writable again. No thread is blocked in `accept` or in a
//! read or write, and an idle server wakes nothing. A registered listener keeps the event loop
//! from going idle until `shutdown`.
//!
//! ## What's intentionally missing (v1 — cold-start / low-concurrency focus)
//! - **Concurrency**: none of the limits come from threads; a connection costs its buffers and
//!   one reactor registration. A request that sends nothing for 30 s, and a response the peer
//!   does not read for 30 s, are dropped.
//! - **Keep-alive**: every response is `Connection: close`; one request per connection.
//! - **Streaming**: request and response bodies are fully buffered (no chunked *response*
//!   output) — same limitation the fetch client / body streams have today. The response queue
//!   behind a slow peer is bounded by the body already buffered.
//! - **No HTTP/2, no `Expect: 100-continue`, no trailers on responses, no `Date` header**
//!   (formatting an HTTP-date without a date library is deferred), **no TLS/https** (same
//!   STOP-AND-FLAG as the client: TLS can't be built on std alone).
//! - **Platforms without a readiness reactor** cannot serve: `Lumen.serve` throws.

use std::collections::HashMap;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;
use std::time::Duration;

use lumen::embed::{Deferred, OpResult};
use lumen_bind::NativeError;
use lumen_common::http_body::{Decoder, Framing};
use lumen_host::net::{
    headers_entries, read_served_body, request_header, server_request, served_response,
    ServedResponse,
};
use lumen_host::{Ctx, OpError, Value};
use lumen_os::reactor::Interest;

use crate::http::{INITIAL_BODY_CAPACITY, MAX_BODY};
use crate::nbio::{self, write_some, Conn as NbConn, Fill, Watch};
use crate::websocket::{adopt_connection, close_socket, send_frame};
use crate::websocket_class::Outgoing;

/// A client that sends nothing for this long while its request is incomplete is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Request-line + headers size cap (a hostile client can't balloon the server).
const MAX_HEADER_BYTES: usize = crate::http::MAX_HEADER_BYTES;
/// Connections accepted per wake before the loop gets a turn.
const ACCEPT_BATCH: usize = 64;
/// Largest chunk the chunked-body decoder hands back at once.
const BODY_CHUNK: usize = 64 << 10;

/// Live servers and the connections awaiting an answer. Lives in `OpState`. Script values (the
/// handlers) are `!Send` and so only ever touched on the loop thread.
#[derive(Default)]
pub(crate) struct ServerRegistry {
    next: u64,
    servers: HashMap<u64, ServerEntry>,
    /// Accepted connections whose request is still arriving.
    requests: HashMap<u64, Request>,
    /// Answered connections whose response the socket has not taken yet.
    responses: HashMap<u64, NbConn<TcpStream>>,
    conns: HashMap<u32, Conn>,
    /// The hidden slot on a delivered `Request` that holds its connection id.
    conn_slot: Option<String>,
    /// Receives the completions whose work is done by their decoder.
    noop: Option<Value>,
}

struct ServerEntry {
    /// Declared before the listener: it deregisters the listener before the socket closes.
    watch: Watch,
    listener: TcpListener,
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

/// An accepted connection whose request is still being read.
struct Request {
    conn: NbConn<TcpStream>,
    parser: RequestParser,
    peer: SocketAddr,
    server_id: u64,
}

/// A fully parsed request.
struct Parsed {
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

pub(crate) fn noop(ctx: &mut Ctx) -> Value {
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
    listener
        .set_nonblocking(true)
        .map_err(|e| NativeError::runtime(format!("listen {hostname}:{port}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| NativeError::runtime(format!("local_addr: {e}")))?;
    let handler = Rc::new(Handler { call, this, on_error });
    let finished = Deferred::new(ctx);
    let promise = finished.promise();

    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        id
    };
    let idle = noop(ctx);
    let watch = Watch::open(
        ctx,
        nbio::listener_source(&listener),
        Interest::READ,
        id,
        idle,
        decode_accept,
    )
    .map_err(|e| NativeError::runtime(format!("listen {hostname}:{port}: {e}")))?;
    registry(ctx).servers.insert(
        id,
        ServerEntry {
            watch,
            listener,
            handler,
            finished: Some(finished),
        },
    );

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

/// Stops the listener: the next pass of its task closes it and resolves `finished`.
fn close_server(ctx: &mut Ctx, id: u64) {
    if let Some(entry) = registry(ctx).servers.get(&id) {
        entry.watch.link().remote().raise(nbio::CLOSE);
    }
}

/// Close every listener the realm still holds, and drop the connections in flight.
pub(crate) fn close_all(ctx: &mut Ctx) {
    let (servers, requests, responses) = match ctx.host_mut::<ServerRegistry>() {
        Some(reg) => (
            reg.servers.drain().map(|(_, e)| e).collect::<Vec<_>>(),
            reg.requests.drain().map(|(_, r)| r).collect::<Vec<_>>(),
            reg.responses.drain().map(|(_, r)| r).collect::<Vec<_>>(),
        ),
        None => return,
    };
    for entry in servers {
        entry.watch.link().cancel(ctx);
    }
    for request in requests {
        request.conn.link().cancel(ctx);
    }
    for response in responses {
        response.link().cancel(ctx);
    }
}

// ---- completion decoders (run on the loop thread with &mut Ctx) --------------------------------

/// Settles one wake of a listener: accepts what is waiting, or closes the server.
fn decode_accept(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let id = *payload.downcast::<u64>().expect("accept payload");
    let Some(entry) = registry(ctx).servers.get(&id) else {
        return Ok(Vec::new());
    };
    let link = entry.watch.link().clone();
    let flags = link.take();

    let mut accepted = Vec::new();
    let mut ended = flags & nbio::CLOSE != 0;
    let mut more = false;
    if !ended {
        loop {
            if accepted.len() == ACCEPT_BATCH {
                more = true;
                break;
            }
            match entry.listener.accept() {
                Ok(pair) => accepted.push(pair),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(_) => {
                    ended = true;
                    break;
                }
            }
        }
    }

    if ended {
        let finished = registry(ctx)
            .servers
            .remove(&id)
            .and_then(|entry| entry.finished);
        link.cancel(ctx);
        if let Some(finished) = finished {
            finished.resolve(ctx, Value::Undefined);
        }
    }
    for (stream, peer) in accepted {
        start_request(ctx, id, stream, peer);
    }
    if ended {
        return Ok(Vec::new());
    }
    if more {
        link.mark(nbio::RESUME);
    }
    link.next_task(ctx);
    if let Some(entry) = registry(ctx).servers.get(&id) {
        if entry.watch.rearm(Interest::READ).is_err() {
            link.remote().raise(nbio::CLOSE);
        }
    }
    Ok(Vec::new())
}

/// Registers an accepted connection to read its request on the loop.
fn start_request(ctx: &mut Ctx, server_id: u64, stream: TcpStream, peer: SocketAddr) {
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    stream.set_nodelay(true).ok();
    let fallback_host = stream
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        id
    };
    let idle = noop(ctx);
    let Ok(mut conn) = NbConn::open(ctx, stream, Interest::READ, id, idle, decode_request) else {
        return;
    };
    conn.watch_reads(READ_TIMEOUT);
    registry(ctx).requests.insert(
        id,
        Request {
            conn,
            parser: RequestParser::new(fallback_host),
            peer,
            server_id,
        },
    );
}

/// What one pass over a connection's request found.
enum Progress {
    Wait,
    Ready,
    Bad(String),
    Drop,
}

fn read_request(request: &mut Request, flags: u8) -> Progress {
    if flags & nbio::READ_STALL != 0 && request.conn.on_stall(flags).is_some() {
        return Progress::Drop;
    }
    let (mut eof, mut more) = (false, false);
    match request.conn.fill() {
        Ok(Fill::Eof) => eof = true,
        Ok(Fill::More) => more = true,
        Ok(Fill::Drained) => {}
        Err(_) => return Progress::Drop,
    }
    match request.parser.advance(request.conn.rx.bytes(), eof) {
        Ok((used, done)) => {
            request.conn.rx.consume(used);
            if done {
                Progress::Ready
            } else if eof {
                Progress::Drop
            } else {
                if more {
                    request.conn.link().mark(nbio::RESUME);
                }
                Progress::Wait
            }
        }
        Err(RequestError::Empty) => Progress::Drop,
        Err(RequestError::Bad(message)) => Progress::Bad(message),
    }
}

/// Settles one wake of a connection that is still sending its request.
fn decode_request(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let id = *payload.downcast::<u64>().expect("request payload");
    let Some(request) = registry(ctx).requests.get_mut(&id) else {
        return Ok(Vec::new());
    };
    let link = request.conn.link();
    let flags = link.take();
    match read_request(request, flags) {
        Progress::Wait => {
            link.next_task(ctx);
            let rearmed = registry(ctx)
                .requests
                .get(&id)
                .is_some_and(|r| r.conn.rearm(true).is_ok());
            if !rearmed {
                registry(ctx).requests.remove(&id);
                link.cancel(ctx);
            }
        }
        Progress::Drop => {
            registry(ctx).requests.remove(&id);
            link.cancel(ctx);
        }
        Progress::Bad(message) => {
            let request = registry(ctx).requests.remove(&id).expect("request exists");
            link.cancel(ctx);
            let stream = request.conn.into_stream();
            let _ = write_simple(&stream, 400, "Bad Request", message.as_bytes());
        }
        Progress::Ready => {
            let Request {
                conn,
                parser,
                peer,
                server_id,
            } = registry(ctx).requests.remove(&id).expect("request exists");
            link.cancel(ctx);
            let stream = conn.into_stream();
            let handler = registry(ctx)
                .servers
                .get(&server_id)
                .map(|entry| entry.handler.clone());
            if let Some(handler) = handler {
                dispatch_request(ctx, &handler, stream, peer, parser.finish());
            }
        }
    }
    Ok(Vec::new())
}

fn dispatch_request(
    ctx: &mut Ctx,
    handler: &Rc<Handler>,
    stream: TcpStream,
    peer: SocketAddr,
    request: Parsed,
) {
    let conn_id = ctx.resource_table().add(stream);
    registry(ctx).conns.insert(
        conn_id,
        Conn {
            remote: peer.ip().to_string(),
            upgraded: false,
            head_request: request.method.eq_ignore_ascii_case("HEAD"),
        },
    );
    deliver(
        ctx,
        handler,
        conn_id,
        peer,
        &request.method,
        &request.url,
        &request.headers,
        request.body,
    );
}

/// Settles one wake of a connection whose response is still going out.
fn decode_response(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let id = *payload.downcast::<u64>().expect("response payload");
    let Some(conn) = registry(ctx).responses.get_mut(&id) else {
        return Ok(Vec::new());
    };
    let link = conn.link();
    let flags = link.take();
    let stalled = flags & nbio::WRITE_STALL != 0 && conn.on_stall(flags).is_some();
    let flushed = !stalled && conn.flush().is_ok();
    if flushed && conn.pending() {
        link.next_task(ctx);
        let rearmed = registry(ctx)
            .responses
            .get(&id)
            .is_some_and(|c| c.rearm(false).is_ok());
        if rearmed {
            return Ok(Vec::new());
        }
    }
    let conn = registry(ctx).responses.remove(&id).expect("response exists");
    link.cancel(ctx);
    if flushed && !conn.pending() {
        let _ = conn.into_stream().shutdown(Shutdown::Both);
    }
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

/// Serialize the response and write it to the socket, then close it. The socket is taken out of
/// the resource table here, so a connection already answered or upgraded is left alone. What the
/// socket does not take at once is queued behind a writable registration.
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
    let Some(mut stream) = stream else {
        return;
    };
    let bytes = build_response(head.status, &head.status_text, &head.headers, &body, head_request);
    let Ok(sent) = write_some(&mut stream, &bytes) else {
        return;
    };
    if sent == bytes.len() {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        id
    };
    let idle = noop(ctx);
    let Ok(mut conn) = NbConn::open(ctx, stream, Interest::WRITE, id, idle, decode_response) else {
        return;
    };
    if conn.write(&bytes[sent..]).is_err() {
        conn.link().cancel(ctx);
        return;
    }
    registry(ctx).responses.insert(id, conn);
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

enum RequestError {
    /// The peer closed before sending anything.
    Empty,
    Bad(String),
}

fn bad(message: impl Into<String>) -> RequestError {
    RequestError::Bad(message.into())
}

enum Stage {
    Head {
        request_line: Option<String>,
        headers: Vec<(String, String)>,
        budget: usize,
    },
    Body {
        head: Parsed,
        body: BodyKind,
    },
    Done(Parsed),
}

enum BodyKind {
    Length(usize),
    Chunked(Decoder),
}

/// Incremental request parser: the request line and headers line by line, then a `Content-Length`
/// or chunked body, fed with whatever bytes have arrived.
struct RequestParser {
    stage: Option<Stage>,
    fallback_host: String,
}

impl RequestParser {
    fn new(fallback_host: String) -> Self {
        RequestParser {
            stage: Some(Stage::Head {
                request_line: None,
                headers: Vec::new(),
                budget: MAX_HEADER_BYTES,
            }),
            fallback_host,
        }
    }

    /// Consumes what it can of `input`; returns how much it took and whether the request is
    /// complete. `eof` says no more bytes will arrive.
    fn advance(&mut self, input: &[u8], eof: bool) -> Result<(usize, bool), RequestError> {
        let mut at = 0;
        loop {
            match self.stage.take().expect("parser has a stage") {
                Stage::Head {
                    mut request_line,
                    mut headers,
                    mut budget,
                } => {
                    let rest = &input[at..];
                    let (line, used) = match rest.iter().position(|&b| b == b'\n') {
                        Some(end) => (&rest[..=end], end + 1),
                        None if rest.len() > budget => return Err(bad("headers too large")),
                        None if eof => (rest, rest.len()),
                        None => {
                            self.stage = Some(Stage::Head {
                                request_line,
                                headers,
                                budget,
                            });
                            return Ok((at, false));
                        }
                    };
                    if used > budget {
                        return Err(bad("headers too large"));
                    }
                    budget -= used;
                    at += used;
                    let text = String::from_utf8_lossy(line);
                    let text = text.trim_end();
                    match &request_line {
                        None => {
                            if line.is_empty() {
                                return Err(RequestError::Empty);
                            }
                            request_line = Some(text.to_string());
                        }
                        Some(_) if line.is_empty() || text.is_empty() => {
                            let head = self.finish_head(request_line.take().unwrap(), headers)?;
                            self.stage = Some(self.body_stage(head)?);
                            continue;
                        }
                        Some(_) => {
                            if let Some(i) = text.find(':') {
                                headers.push((text[..i].to_string(), text[i + 1..].trim().to_string()));
                            }
                        }
                    }
                    self.stage = Some(Stage::Head {
                        request_line,
                        headers,
                        budget,
                    });
                }
                Stage::Body { mut head, mut body } => {
                    let rest = &input[at..];
                    let done = match &mut body {
                        BodyKind::Length(need) => {
                            let take = rest.len().min(*need - head.body.len());
                            head.body.extend_from_slice(&rest[..take]);
                            at += take;
                            if head.body.len() == *need {
                                true
                            } else if eof {
                                return Err(bad("read body: unexpected end of stream"));
                            } else {
                                false
                            }
                        }
                        BodyKind::Chunked(decoder) => {
                            let mut offset = 0;
                            loop {
                                let step = decoder
                                    .decode(&rest[offset..], eof, BODY_CHUNK)
                                    .map_err(|e| bad(format!("chunked body: {e}")))?;
                                offset += step.consumed;
                                let progress = step.consumed > 0 || step.chunk.is_some();
                                if let Some(chunk) = step.chunk {
                                    head.body.extend_from_slice(&chunk);
                                }
                                if step.done || !progress {
                                    break;
                                }
                            }
                            at += offset;
                            decoder.is_done()
                        }
                    };
                    if done {
                        self.stage = Some(Stage::Done(head));
                        continue;
                    }
                    self.stage = Some(Stage::Body { head, body });
                    return Ok((at, false));
                }
                Stage::Done(head) => {
                    self.stage = Some(Stage::Done(head));
                    return Ok((at, true));
                }
            }
        }
    }

    fn finish_head(
        &self,
        request_line: String,
        headers: Vec<(String, String)>,
    ) -> Result<Parsed, RequestError> {
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("/").to_string();
        if method.is_empty() {
            return Err(bad("malformed request line"));
        }
        let host = header(&headers, "host").unwrap_or_else(|| self.fallback_host.clone());
        let url = if target.starts_with("http://") || target.starts_with("https://") {
            target // absolute-form (proxy requests)
        } else if target.starts_with('/') {
            format!("http://{host}{target}")
        } else {
            // authority-form (CONNECT) or asterisk-form: not meaningfully routable; give a URL
            // the Request constructor still accepts.
            format!("http://{host}/{target}")
        };
        Ok(Parsed {
            method: method.to_ascii_uppercase(),
            url,
            headers,
            body: Vec::new(),
        })
    }

    fn body_stage(&self, head: Parsed) -> Result<Stage, RequestError> {
        if head.method == "GET" || head.method == "HEAD" {
            return Ok(Stage::Done(head));
        }
        if header(&head.headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            let decoder = Decoder::new(Framing::Chunked, MAX_BODY)
                .map_err(|e| bad(format!("chunked body: {e}")))?;
            return Ok(Stage::Body {
                head,
                body: BodyKind::Chunked(decoder),
            });
        }
        // No framing on a request means no body (unlike a response, there is no read-to-EOF).
        match header(&head.headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
            Some(len) if len > 0 => {
                let need = len.min(MAX_BODY) as usize;
                let mut head = head;
                head.body = Vec::with_capacity(need.min(INITIAL_BODY_CAPACITY as usize));
                Ok(Stage::Body {
                    head,
                    body: BodyKind::Length(need),
                })
            }
            _ => Ok(Stage::Done(head)),
        }
    }

    fn finish(self) -> Parsed {
        match self.stage {
            Some(Stage::Done(head)) => head,
            _ => unreachable!("finish before the request was complete"),
        }
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

/// A bare status-only response for the 400 path, sent best effort on the nonblocking socket.
fn write_simple(stream: &TcpStream, status: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    let mut s = stream;
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    let sent = write_some(&mut s, &response);
    let _ = stream.shutdown(Shutdown::Both);
    sent.map(drop)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> Result<Parsed, String> {
        let mut parser = RequestParser::new("fallback:1".into());
        match parser.advance(raw, true) {
            Ok((_, true)) => Ok(parser.finish()),
            Ok((_, false)) => Err("incomplete".into()),
            Err(RequestError::Empty) => Err("empty".into()),
            Err(RequestError::Bad(message)) => Err(message),
        }
    }

    #[test]
    fn chunked_request_body_is_decoded() {
        let raw = b"POST /up HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let request = parse(raw).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "http://h/up");
        assert_eq!(request.body, b"Wikipedia");
    }

    #[test]
    fn request_fed_byte_by_byte_matches_one_shot() {
        let raw = b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nhello";
        let mut parser = RequestParser::new(String::new());
        let mut buffered = Vec::new();
        let mut done = false;
        for byte in raw {
            buffered.push(*byte);
            let (used, finished) = parser.advance(&buffered, false).ok().unwrap();
            buffered.drain(..used);
            done = finished;
        }
        assert!(done);
        assert!(buffered.is_empty());
        assert_eq!(parser.finish().body, b"hello");
    }

    #[test]
    fn oversized_chunk_size_line_is_rejected() {
        let mut raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'1', 100_000));
        raw.extend_from_slice(b"\r\n");
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn huge_content_length_with_small_body_does_not_preallocate() {
        let raw = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\ntiny", u64::MAX);
        let mut parser = RequestParser::new(String::new());
        assert!(parser.advance(raw.as_bytes(), true).is_err());

        let raw = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello world";
        let mut parser = RequestParser::new(String::new());
        let (used, done) = parser.advance(raw, true).ok().unwrap();
        assert!(done);
        assert_eq!(raw.len() - used, " world".len());
        let body = parser.finish().body;
        assert_eq!(body, b"hello");
        assert!(body.capacity() <= INITIAL_BODY_CAPACITY as usize);
    }

    #[test]
    fn endless_header_is_rejected_at_the_cap() {
        let mut raw = b"GET / HTTP/1.1\r\nX: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', MAX_HEADER_BYTES + 10));
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn closed_connection_without_bytes_is_an_empty_request() {
        assert_eq!(parse(b"").err().unwrap(), "empty");
    }
}
