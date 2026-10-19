//! WebSocket (RFC 6455) over plain TCP or verified TLS: a *client* (the upgrade sibling of the fetch
//! client in `http.rs`, driving the native `WebSocket` class in `websocket_class.rs`) plus a *server-side
//! adopt* (`adopt_connection`), which takes a connection accepted by the HTTP server in `server.rs`,
//! answers the 101 handshake, and runs it through the same registry/frame machinery with unmasked
//! outgoing frames — backing `Lumen.upgradeWebSocket` and Bun.serve's `websocket` option.
//!
//! ## How it runs on the loop
//! `connect` runs the TCP/TLS dial + HTTP upgrade handshake on a pool thread (one short, bounded
//! job that ends with the socket switched to nonblocking mode) and comes back as a completion; its
//! decoder fires the socket's JS dispatch (`"open"`) and registers the socket with the loop's
//! readiness reactor (`nbio`). An adopted server connection is registered at once. From then on a
//! connection has no thread and no timer while it is idle: the reactor wakes the loop thread, which
//! reads what the socket has until it would block, answers pings, swallows pongs and delivers at
//! most one message per loop completion (a message left in the buffer schedules another pass), so
//! every message keeps its own microtask checkpoint. Sends and closes write straight to the
//! socket on the loop thread; what the socket does not take is queued and goes out when the
//! reactor reports it writable again, and a peer that takes nothing for 30 s is dropped.
//!
//! ## What's intentionally missing (v1)
//! - **permessage-deflate** and other extensions (`extensions` is always `""`).
//! - **bufferedAmount**: `send` always reports 0 buffered; the queue behind a slow peer is capped
//!   at `MAX_QUEUED`, beyond which `send` throws.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;

use lumen_bind::NativeError;
use lumen_host::{Ctx, SpawnHandle, Value};
use lumen_os::reactor::Interest;

use crate::nbio::{self, Conn, Fill, HeadError, Link, NbStream};
use crate::url;
use crate::websocket_class::Outgoing;
use lumen_common::hash::{digest, Algo};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Message size cap (mirrors the HTTP body cap); exceeding it fails the connection with 1009.
const MAX_MESSAGE: usize = 32 << 20;
/// Output queued behind a peer that is not reading; `send` throws beyond it.
const MAX_QUEUED: usize = 64 << 20;

pub(crate) fn base64(data: &[u8]) -> String {
    lumen_common::codec::base64_encode(data, false, true)
}

/// The `Sec-WebSocket-Accept` value for a handshake `Sec-WebSocket-Key` (RFC 6455 §4.2.2 step
/// 5.4). Public: a WebSocket-capable server upgrade (and this crate's own tests) need it too.
pub fn websocket_accept(key: &str) -> String {
    let mut input = key.trim().to_string();
    input.push_str(GUID);
    base64(&digest(Algo::Sha1, input.as_bytes()))
}

// ---- frame codec -------------------------------------------------------------------------------

/// One decoded event from the wire, message-level (fragments already assembled).
#[derive(Debug, PartialEq)]
pub(crate) enum WsEvent {
    Text(String),
    Binary(Vec<u8>),
    /// Peer close frame: `(code, reason)`; 1005 = no code present.
    Close(u16, String),
}

/// Why a read loop ended without a clean message.
pub(crate) enum WsError {
    /// Protocol violation → fail the connection with this close code.
    Protocol(u16, &'static str),
    /// The socket died (EOF/reset/timeout).
    Io(String),
}

/// Encode one frame. Client frames are ALWAYS masked (RFC 6455 §5.3); `mask` comes from the
/// caller so the codec stays deterministic under test.
pub(crate) fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0f)); // FIN + opcode
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= 0xffff {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Encode one UNMASKED frame — the server side of the wire (a server MUST NOT mask, RFC 6455
/// §5.1). Used by connections adopted via `adopt_connection` (`Lumen.serve` → WebSocket handoff).
pub(crate) fn encode_frame_unmasked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0f)); // FIN + opcode
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// A close frame's payload (code + UTF-8 reason).
pub(crate) fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = code.to_be_bytes().to_vec();
    p.extend_from_slice(reason.as_bytes());
    p
}

struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

enum Parsed {
    /// A whole frame and the number of bytes it took.
    Frame(RawFrame, usize),
    /// At least this many more bytes are needed to make progress.
    Need(usize),
}

/// Decode the frame at the start of `buf`, if it is all there. The size limits are checked as soon
/// as the length is known, before any payload is waited for.
fn parse_frame(buf: &[u8], max: usize) -> Result<Parsed, WsError> {
    if buf.len() < 2 {
        return Ok(Parsed::Need(2 - buf.len()));
    }
    let fin = buf[0] & 0x80 != 0;
    if buf[0] & 0x70 != 0 {
        return Err(WsError::Protocol(
            1002,
            "reserved bits set (no extension negotiated)",
        ));
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as usize;
    if opcode >= 0x8 && (!fin || len > 125) {
        return Err(WsError::Protocol(1002, "malformed control frame"));
    }
    let mut head = 2;
    if len == 126 {
        head = 4;
        if buf.len() < head {
            return Ok(Parsed::Need(head - buf.len()));
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    } else if len == 127 {
        head = 10;
        if buf.len() < head {
            return Ok(Parsed::Need(head - buf.len()));
        }
        let n = u64::from_be_bytes(buf[2..10].try_into().unwrap());
        if n > max as u64 {
            return Err(WsError::Protocol(1009, "message too big"));
        }
        len = n as usize;
    }
    if len > max {
        return Err(WsError::Protocol(1009, "message too big"));
    }
    // A server MUST NOT mask (§5.1); tolerate it by unmasking rather than failing.
    if masked {
        head += 4;
    }
    let total = head + len;
    if buf.len() < total {
        return Ok(Parsed::Need(total - buf.len()));
    }
    let mut payload = buf[head..total].to_vec();
    if masked {
        let mask: [u8; 4] = buf[head - 4..head].try_into().unwrap();
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok(Parsed::Frame(
        RawFrame {
            fin,
            opcode,
            payload,
        },
        total,
    ))
}

/// Read one frame from a blocking reader, taking exactly the bytes it needs.
fn read_raw_frame(r: &mut impl Read, max: usize) -> Result<RawFrame, WsError> {
    let mut buf = Vec::new();
    loop {
        match parse_frame(&buf, max)? {
            Parsed::Frame(frame, _) => return Ok(frame),
            Parsed::Need(count) => {
                let at = buf.len();
                buf.resize(at + count, 0);
                r.read_exact(&mut buf[at..])
                    .map_err(|e| WsError::Io(e.to_string()))?;
            }
        }
    }
}

/// Mask keys need unpredictability only against proxies (RFC 6455 §10.3); a cheap LCG seeded from
/// the handshake's CSPRNG key is fine.
fn next_mask(seed: &mut u64) -> [u8; 4] {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 24).to_be_bytes()[4..8].try_into().unwrap()
}

fn finish_message(opcode: u8, payload: Vec<u8>) -> Result<WsEvent, WsError> {
    if opcode == 0x1 {
        String::from_utf8(payload)
            .map(WsEvent::Text)
            .map_err(|_| WsError::Protocol(1007, "text message is not UTF-8"))
    } else {
        Ok(WsEvent::Binary(payload))
    }
}

// ---- registry + ops ----------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WsRegistry {
    next: u64,
    socks: HashMap<u64, WsEntry>,
}

/// An open socket on the reactor.
struct Live {
    conn: Conn<Box<dyn NbStream>>,
    /// The message being assembled from fragments: its opcode and the bytes so far.
    partial: Option<(u8, Vec<u8>)>,
    /// The peer closed its end; buffered frames are still delivered.
    eof: bool,
    /// A read or rearm error to report once the buffered frames are delivered.
    failure: Option<String>,
}

impl Live {
    fn wants_read(&self) -> bool {
        !self.eof && self.failure.is_none()
    }

    /// Waits for writability when the socket did not take everything.
    fn watch_writable(&self) {
        if self.conn.pending() {
            let _ = self.conn.rearm(self.wants_read());
        }
    }
}

struct WsEntry {
    /// `None` until the handshake completes.
    live: Option<Live>,
    /// One close frame ever goes out (ours or the echo of theirs).
    close_sent: bool,
    /// Nobody listens to the socket any more: it lives on until the peer answers our close.
    dead: bool,
    dispatch: Value,
    mask_seed: u64,
    /// true = client end (outgoing frames masked); false = a connection adopted server-side.
    masked: bool,
}

/// What the connect task sends back to the loop.
struct ConnectResult {
    id: u64,
    outcome: Result<ConnectedSocket, String>,
}

struct ConnectedSocket {
    /// Nonblocking, positioned right after the handshake response.
    stream: Box<dyn NbStream>,
    protocol: String,
}

fn ws_registry(ctx: &mut Ctx) -> &mut WsRegistry {
    ctx.host_mut::<WsRegistry>()
        .expect("web installs WsRegistry")
}

/// Adopts a connection accepted by `Lumen.serve` (see server.rs: the parsed request's `TcpStream`
/// sits in the resource table under `conn_id`) as a SERVER-side WebSocket: writes the RFC 6455 101
/// handshake response, then joins the same registry/frame machinery the client uses - with
/// `masked: false`, since a server must not mask (§5.1). `dispatch(kind, ...)` receives
/// `("text", string)`, `("binary", u8array)`, `("close", code, reason, wasClean)`,
/// `("fail", code, msg)` (protocol violation), and `("io", msg)` (socket died). There is no
/// "open" event: the connection is open the moment this returns. Returns the socket's id.
pub(crate) fn adopt_connection(
    ctx: &mut Ctx,
    conn_id: u32,
    key: &str,
    protocol: &str,
    extra_headers: &[(String, String)],
    dispatch: Value,
) -> Result<u64, NativeError> {
    if !dispatch.is_callable() {
        return Err(NativeError::type_error("upgrade: dispatch must be a function"));
    }

    // Take the socket out of the resource table (the same handoff the response write makes, so a
    // later response on this connection correctly fails as "already answered").
    let stream = ctx
        .resource_table()
        .close(conn_id)
        .and_then(|rc| rc.downcast::<TcpStream>().ok())
        .and_then(|rc| std::rc::Rc::try_unwrap(rc).ok());
    let Some(stream) = stream else {
        return Err(NativeError::type_error("upgrade: unknown or already-answered connection"));
    };
    stream.set_nodelay(true).ok();

    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n",
        websocket_accept(key)
    );
    if !protocol.is_empty() {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    for (name, value) in extra_headers {
        // The handshake-critical headers above must not be overridden by user extras.
        if name.eq_ignore_ascii_case("upgrade")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("sec-websocket-accept")
        {
            continue;
        }
        resp.push_str(&format!("{name}: {value}\r\n"));
    }
    resp.push_str("\r\n");

    let id = {
        let reg = ws_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        id
    };
    let mut conn = Conn::open(
        ctx,
        Box::new(stream) as Box<dyn NbStream>,
        Interest::READ,
        id,
        dispatch.clone(),
        decode_ws,
    )
    .map_err(|e| NativeError::runtime(format!("WebSocket upgrade: {e}")))?;
    if let Err(e) = conn.write(resp.as_bytes()) {
        conn.link().cancel(ctx);
        return Err(NativeError::runtime(format!(
            "WebSocket upgrade: handshake write: {e}"
        )));
    }
    let live = Live {
        conn,
        partial: None,
        eof: false,
        failure: None,
    };
    live.watch_writable();
    ws_registry(ctx).socks.insert(
        id,
        WsEntry {
            live: Some(live),
            close_sent: false,
            dead: false,
            dispatch,
            mask_seed: 0,
            masked: false,
        },
    );
    Ok(id)
}

/// Open a client socket to `target` offering `protocols`. The handshake runs on the pool; the
/// socket's lifecycle then flows through `dispatch(kind, ...)`: `("open", protocol)`,
/// `("text", string)`, `("binary", u8array)`, `("close", code, reason, wasClean)`,
/// `("fail", code, message)` and `("error", message)`. Returns the socket's id.
pub(crate) fn connect_socket(
    ctx: &mut Ctx,
    target: &str,
    protocols: &[String],
    dispatch: Value,
) -> Result<u64, NativeError> {
    if !dispatch.is_callable() {
        return Err(NativeError::type_error("connect: dispatch must be a function"));
    }

    let u = url::parse(target, None).map_err(|e| NativeError::named("SyntaxError", e))?;
    match u.scheme.as_str() {
        "ws" | "wss" => {}
        other => {
            return Err(NativeError::named(
                "SyntaxError",
                format!("WebSocket: unsupported scheme '{other}'"),
            ))
        }
    }

    // 16 random bytes for the handshake key (CSPRNG); also seeds the per-socket mask LCG.
    let key_bytes = crate::web_random_bytes(16).map_err(|e| NativeError::runtime(e.message().to_string()))?;
    let key = base64(&key_bytes);
    let mask_seed = u64::from_be_bytes(key_bytes[0..8].try_into().unwrap()) | 1;

    let id = {
        let reg = ws_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.socks.insert(
            id,
            WsEntry {
                live: None,
                close_sent: false,
                dead: false,
                dispatch: dispatch.clone(),
                mask_seed,
                masked: true,
            },
        );
        id
    };

    let task = lumen_host::register_task(ctx, dispatch, None, decode_connect);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    let protocols = protocols.join(", ");
    spawn.spawn_blocking(task, move || {
        Box::new(ConnectResult {
            id,
            outcome: handshake(&u, &key, &protocols),
        })
    });
    Ok(id)
}

/// Encode and send one data frame. `false` when the socket is gone or already closing.
pub(crate) fn send_frame(ctx: &mut Ctx, id: u64, payload: Outgoing<'_>) -> Result<bool, NativeError> {
    let (opcode, bytes) = match payload {
        Outgoing::Binary(bytes) => (0x2u8, bytes),
        Outgoing::Text(text) => (0x1u8, text.as_bytes()),
    };
    let reg = ws_registry(ctx);
    let Some(e) = reg.socks.get_mut(&id) else {
        return Ok(false);
    };
    if e.close_sent {
        return Ok(false);
    }
    // Server-adopted sockets never mask (RFC 6455 §5.1).
    let mask = e.masked.then(|| next_mask(&mut e.mask_seed));
    let Some(live) = e.live.as_mut() else {
        return Err(NativeError::runtime("send before open"));
    };
    let frame = match mask {
        Some(m) => encode_frame(opcode, bytes, m),
        None => encode_frame_unmasked(opcode, bytes),
    };
    if live.conn.queued() + frame.len() > MAX_QUEUED {
        return Err(NativeError::runtime("WebSocket send: write buffer is full"));
    }
    live.conn
        .write(&frame)
        .map_err(|e| NativeError::runtime(format!("WebSocket send: {e}")))?;
    live.watch_writable();
    Ok(true)
}

/// Sends the one close frame a socket may send. The peer's echo then arrives as the close event.
fn send_close(entry: &mut WsEntry, payload: &[u8], mask: [u8; 4]) {
    if std::mem::replace(&mut entry.close_sent, true) {
        return;
    }
    let Some(live) = entry.live.as_mut() else {
        return;
    };
    let frame = if entry.masked {
        encode_frame(0x8, payload, mask)
    } else {
        encode_frame_unmasked(0x8, payload)
    };
    let _ = live.conn.write(&frame);
    live.watch_writable();
}

/// Send the close frame (once); the read loop then surfaces the peer's echo as the close event.
pub(crate) fn close_socket(ctx: &mut Ctx, id: u64, code: u16, reason: &str) {
    if let Some(entry) = ws_registry(ctx).socks.get_mut(&id) {
        send_close(entry, &close_payload(code, reason), [0x1f, 0x2e, 0x3d, 0x4c]);
    }
}

/// Close a socket nobody listens to any more: it ends with the peer's echo, silently.
pub(crate) fn abandon_socket(ctx: &mut Ctx, id: u64) {
    close_socket(ctx, id, 1001, "");
    let reg = ws_registry(ctx);
    match reg.socks.get_mut(&id) {
        Some(entry) if entry.live.is_some() => entry.dead = true,
        _ => {
            reg.socks.remove(&id);
        }
    }
}

/// Dial + HTTP/1.1 upgrade (RFC 6455 §4.1/§4.2) on a blocking socket, which is switched to
/// nonblocking mode once the response head is read. Returns the open stream and the negotiated
/// subprotocol ("" when none).
fn handshake(u: &url::Url, key: &str, protocols: &str) -> Result<ConnectedSocket, String> {
    let port = u.port.unwrap_or(if u.scheme == "wss" { 443 } else { 80 });
    let host = u.hostname().trim_matches(['[', ']']);
    let tcp = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_nodelay(true).ok();
    tcp.set_write_timeout(Some(nbio::HANDSHAKE_TIMEOUT)).ok();
    tcp.set_read_timeout(Some(nbio::HANDSHAKE_TIMEOUT)).ok();
    if u.scheme == "wss" {
        let mut tls = lumen_tls::TlsStream::connect(tcp, host)?;
        let protocol = upgrade(&mut tls, u, key, protocols)?;
        tls.set_nonblocking(true)
            .map_err(|e| format!("handshake: {e}"))?;
        Ok(ConnectedSocket {
            stream: Box::new(tls),
            protocol,
        })
    } else {
        let mut tcp = tcp;
        let protocol = upgrade(&mut tcp, u, key, protocols)?;
        tcp.set_nonblocking(true)
            .map_err(|e| format!("handshake: {e}"))?;
        Ok(ConnectedSocket {
            stream: Box::new(tcp),
            protocol,
        })
    }
}

/// The upgrade request and the check of its response; returns the negotiated subprotocol.
fn upgrade(
    stream: &mut (impl Read + Write),
    u: &url::Url,
    key: &str,
    protocols: &str,
) -> Result<String, String> {
    let port = u.port.unwrap_or(if u.scheme == "wss" { 443 } else { 80 });
    let host_header = if u.port.is_some() && u.port != Some(80) {
        format!("{}:{}", u.hostname(), port)
    } else {
        u.hostname().to_string()
    };
    let path = u.request_target();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    if !protocols.is_empty() {
        req.push_str(&format!("Sec-WebSocket-Protocol: {protocols}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("handshake write: {e}"))?;

    let head = nbio::read_head(stream, 64 << 10).map_err(|e| match e {
        HeadError::TooLarge => "handshake response too large".to_string(),
        HeadError::Io(e) => format!("handshake read: {e}"),
    })?;
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
        return Err(format!("handshake refused: {status}"));
    }
    let mut accept = None;
    let mut upgrade_ok = false;
    let mut protocol = String::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("sec-websocket-accept") {
            accept = Some(v.to_string());
        } else if k.eq_ignore_ascii_case("upgrade") {
            upgrade_ok = v.eq_ignore_ascii_case("websocket");
        } else if k.eq_ignore_ascii_case("sec-websocket-protocol") {
            protocol = v.to_string();
        }
    }
    if !upgrade_ok {
        return Err("handshake response missing 'Upgrade: websocket'".into());
    }
    if accept.as_deref() != Some(websocket_accept(key).as_str()) {
        return Err("handshake Sec-WebSocket-Accept mismatch".into());
    }
    // A subprotocol we never offered fails the connection (§4.1 step 5.6).
    if !protocol.is_empty()
        && !protocols
            .split(',')
            .map(str::trim)
            .any(|p| p.eq_ignore_ascii_case(&protocol))
    {
        return Err(format!(
            "server selected unrequested subprotocol '{protocol}'"
        ));
    }
    Ok(protocol)
}

fn decode_connect(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let ConnectResult { id, outcome } = *payload.downcast::<ConnectResult>().expect("ws payload");
    let error = |ctx: &mut Ctx, msg: String| {
        ws_registry(ctx).socks.remove(&id);
        Ok(vec![
            Value::from_string("error".into()),
            Value::from_string(msg),
        ])
    };
    let ConnectedSocket { stream, protocol } = match outcome {
        Ok(socket) => socket,
        Err(msg) => return error(ctx, msg),
    };
    let Some(dispatch) = ws_registry(ctx).socks.get(&id).map(|e| e.dispatch.clone()) else {
        return Ok(vec![Value::from_string("close".into()), Value::Num(1006.0)]);
    };
    let conn = match Conn::open(ctx, stream, Interest::READ, id, dispatch, decode_ws) {
        Ok(conn) => conn,
        Err(msg) => return error(ctx, msg),
    };
    // A TLS session may already hold decrypted bytes the socket will never signal again.
    conn.link().remote().raise(nbio::IO);
    if let Some(entry) = ws_registry(ctx).socks.get_mut(&id) {
        entry.live = Some(Live {
            conn,
            partial: None,
            eof: false,
            failure: None,
        });
    }
    Ok(vec![
        Value::from_string("open".into()),
        Value::from_string(protocol),
    ])
}

/// What one pass over a socket found.
enum Step {
    /// Nothing for JS: a partial frame, a pong, a write that made progress.
    Idle,
    /// A message for JS; the socket stays open.
    Event(Event),
    /// The last report for this socket, which is closed.
    End(Event),
}

enum Event {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
    Fail(u16, &'static str),
    Io(String),
}

/// The next complete message in the receive buffer, answering pings and skipping pongs on the way.
fn next_message(entry: &mut WsEntry) -> Result<Option<WsEvent>, WsError> {
    let WsEntry {
        live,
        mask_seed,
        masked,
        ..
    } = entry;
    let live = live.as_mut().expect("an open socket");
    loop {
        let (frame, used) = match parse_frame(live.conn.rx.bytes(), MAX_MESSAGE)? {
            Parsed::Need(_) => return Ok(None),
            Parsed::Frame(frame, used) => (frame, used),
        };
        live.conn.rx.consume(used);
        match frame.opcode {
            0x9 => {
                // Ping → pong with the same payload (§5.5.3); masked only from the client end.
                let pong = if *masked {
                    encode_frame(0xA, &frame.payload, next_mask(mask_seed))
                } else {
                    encode_frame_unmasked(0xA, &frame.payload)
                };
                live.conn
                    .write(&pong)
                    .map_err(|e| WsError::Io(e.to_string()))?;
                live.watch_writable();
            }
            0xA => {} // unsolicited pong: ignore (§5.5.3)
            0x8 => {
                let (code, reason) = if frame.payload.len() >= 2 {
                    let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                    let reason = String::from_utf8(frame.payload[2..].to_vec())
                        .map_err(|_| WsError::Protocol(1007, "close reason is not UTF-8"))?;
                    (code, reason)
                } else {
                    (1005, String::new())
                };
                return Ok(Some(WsEvent::Close(code, reason)));
            }
            0x1 | 0x2 => {
                if live.partial.is_some() {
                    return Err(WsError::Protocol(
                        1002,
                        "new data frame during fragmented message",
                    ));
                }
                if frame.fin {
                    return finish_message(frame.opcode, frame.payload).map(Some);
                }
                live.partial = Some((frame.opcode, frame.payload));
            }
            0x0 => {
                let Some((op, mut buf)) = live.partial.take() else {
                    return Err(WsError::Protocol(1002, "continuation without a message"));
                };
                if buf.len() + frame.payload.len() > MAX_MESSAGE {
                    return Err(WsError::Protocol(1009, "message too big"));
                }
                buf.extend_from_slice(&frame.payload);
                if frame.fin {
                    return finish_message(op, buf).map(Some);
                }
                live.partial = Some((op, buf));
            }
            _ => return Err(WsError::Protocol(1002, "unknown opcode")),
        }
    }
}

/// One pass over a socket the reactor woke: stalls, queued output, input, then at most one message.
fn advance(entry: &mut WsEntry, link: &Link, flags: u8) -> Step {
    let Some(live) = entry.live.as_mut() else {
        return Step::Idle;
    };
    if flags & (nbio::READ_STALL | nbio::WRITE_STALL) != 0 {
        if let Some(stalled) = live.conn.on_stall(flags) {
            return Step::End(Event::Io(stalled.error().to_string()));
        }
    }
    if live.conn.pending() {
        if let Err(e) = live.conn.flush() {
            return Step::End(Event::Io(e.to_string()));
        }
    }
    let mut more = false;
    if live.wants_read() {
        match live.conn.fill() {
            Ok(Fill::Eof) => live.eof = true,
            Ok(Fill::More) => more = true,
            Ok(Fill::Drained) => {}
            Err(e) => live.failure = Some(e.to_string()),
        }
    }
    let step = match next_message(entry) {
        Ok(Some(WsEvent::Text(s))) => Step::Event(Event::Text(s)),
        Ok(Some(WsEvent::Binary(b))) => Step::Event(Event::Binary(b)),
        Ok(Some(WsEvent::Close(code, reason))) => {
            // Echo the close (once) so the TCP close handshake completes cleanly (§5.5.1).
            let echo = close_payload(if code == 1005 { 1000 } else { code }, "");
            send_close(entry, &echo, [0x37, 0x11, 0x9a, 0x42]);
            Step::End(Event::Close(code, reason))
        }
        Err(WsError::Protocol(code, msg)) => {
            send_close(entry, &close_payload(code, msg), [0x37, 0x11, 0x9a, 0x42]);
            Step::End(Event::Fail(code, msg))
        }
        Err(WsError::Io(msg)) => Step::End(Event::Io(msg)),
        Ok(None) => {
            let live = entry.live.as_ref().expect("an open socket");
            if live.eof {
                Step::End(Event::Io("failed to fill whole buffer".into()))
            } else if let Some(msg) = &live.failure {
                Step::End(Event::Io(msg.clone()))
            } else {
                Step::Idle
            }
        }
    };
    let live = entry.live.as_ref().expect("an open socket");
    let again = match step {
        Step::Event(_) => more || !live.conn.rx.is_empty() || live.eof || live.failure.is_some(),
        Step::Idle => more,
        Step::End(_) => false,
    };
    if again {
        link.mark(nbio::RESUME);
    }
    step
}

fn event_args(ctx: &mut Ctx, event: Event) -> Result<Vec<Value>, Value> {
    let kind = |name: &str| Value::from_string(name.to_string());
    Ok(match event {
        Event::Text(text) => vec![kind("text"), Value::from_string(text)],
        Event::Binary(bytes) => vec![kind("binary"), ctx.make_uint8array(&bytes)?],
        Event::Close(code, reason) => vec![
            kind("close"),
            Value::Num(code as f64),
            Value::from_string(reason),
            Value::Bool(true),
        ],
        Event::Fail(code, msg) => vec![
            kind("fail"),
            Value::Num(code as f64),
            Value::from_string(msg.to_string()),
        ],
        Event::Io(msg) => vec![kind("io"), Value::from_string(msg)],
    })
}

/// Settles one reactor wake of a socket. A wake with nothing to report returns no arguments, which
/// both transports' dispatch functions ignore.
fn decode_ws(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let id = *payload.downcast::<u64>().expect("ws payload");
    let Some(link) = ws_registry(ctx)
        .socks
        .get(&id)
        .and_then(|e| e.live.as_ref())
        .map(|live| live.conn.link())
    else {
        return Ok(Vec::new());
    };
    let flags = link.take();
    let step = match ws_registry(ctx).socks.get_mut(&id) {
        Some(entry) => advance(entry, &link, flags),
        None => return Ok(Vec::new()),
    };
    if let Step::End(event) = step {
        // Dropping the entry deregisters the socket and closes it.
        let dead = ws_registry(ctx).socks.remove(&id).is_some_and(|e| e.dead);
        link.cancel(ctx);
        return if dead { Ok(Vec::new()) } else { event_args(ctx, event) };
    }
    link.next_task(ctx);
    let entry = ws_registry(ctx).socks.get_mut(&id).expect("socket is still open");
    let live = entry.live.as_mut().expect("an open socket");
    if let Err(e) = live.conn.rearm(live.wants_read()) {
        live.failure = Some(e.to_string());
        link.remote().raise(nbio::RESUME);
    }
    match step {
        Step::Event(event) if !entry.dead => event_args(ctx, event),
        _ => Ok(Vec::new()),
    }
}

// ---- test support -------------------------------------------------------------------------------

/// A minimal RFC 6455 echo server for this workspace's tests and benchmarks (NOT part of the
/// runtime): accepts one connection at a time, upgrades, then echoes text/binary messages,
/// answers nothing to pongs, replies to close. `behavior` tweaks let tests exercise edges.
#[doc(hidden)]
#[allow(dead_code)]
pub mod testing {
    use super::*;
    use std::net::TcpListener;

    /// What the echo server should do beyond plain echoing.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        /// Echo every message until the client closes.
        Echo,
        /// Send a ping (expecting the client's transparent pong), then echo.
        PingThenEcho,
        /// Send one fragmented text message ("frag" in 3 parts), then echo.
        FragmentedHello,
        /// Immediately close with (4001, "going away").
        CloseImmediately,
        /// Answer the upgrade with a WRONG Sec-WebSocket-Accept.
        BadAccept,
    }

    /// Spawn the server; returns its port. It serves `conns` connections then exits.
    pub fn spawn_echo(mode: Mode, conns: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..conns {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let _ = serve_one(stream, mode);
            }
        });
        port
    }

    fn serve_one(stream: TcpStream, mode: Mode) -> std::io::Result<()> {
        // Upgrade.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            (&stream).read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let key = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| v.trim().to_string())
            })
            .unwrap_or_default();
        let protocol = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("sec-websocket-protocol")
                .then(|| v.split(',').next().unwrap_or("").trim().to_string())
        });
        let accept = if mode == Mode::BadAccept {
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()
        } else {
            websocket_accept(&key)
        };
        let mut resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n"
        );
        if let Some(p) = protocol.filter(|p| !p.is_empty()) {
            resp.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
        }
        resp.push_str("\r\n");
        (&stream).write_all(resp.as_bytes())?;

        let unmasked = |op: u8, fin: bool, payload: &[u8]| {
            let mut f = vec![if fin { 0x80 | op } else { op }];
            if payload.len() < 126 {
                f.push(payload.len() as u8);
            } else {
                f.push(126);
                f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            f.extend_from_slice(payload);
            f
        };

        match mode {
            Mode::CloseImmediately => {
                let mut p = 4001u16.to_be_bytes().to_vec();
                p.extend_from_slice(b"going away");
                (&stream).write_all(&unmasked(0x8, true, &p))?;
            }
            Mode::PingThenEcho => {
                (&stream).write_all(&unmasked(0x9, true, b"marco"))?;
            }
            Mode::FragmentedHello => {
                (&stream).write_all(&unmasked(0x1, false, b"fr"))?;
                (&stream).write_all(&unmasked(0x0, false, b"agm"))?;
                (&stream).write_all(&unmasked(0x0, true, b"ent"))?;
            }
            _ => {}
        }

        // Echo loop over a buffered reader (frames from the client are masked).
        let mut r = BufReader::new(stream.try_clone()?);
        loop {
            let frame = match read_raw_frame(&mut r, MAX_MESSAGE) {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };
            match frame.opcode {
                0x8 => {
                    (&stream).write_all(&unmasked(0x8, true, &frame.payload))?;
                    return Ok(());
                }
                0x9 => (&stream).write_all(&unmasked(0xA, true, &frame.payload))?,
                0xA => {
                    // A pong: the PingThenEcho handshake completed — tell the client via text.
                    (&stream).write_all(&unmasked(
                        0x1,
                        true,
                        format!("pong:{}", String::from_utf8_lossy(&frame.payload)).as_bytes(),
                    ))?;
                }
                0x1 | 0x2 if frame.fin => {
                    (&stream).write_all(&unmasked(frame.opcode, true, &frame.payload))?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        // Client-masked frames decode back to the payload (server view).
        for (op, payload) in [
            (0x1u8, b"hello".to_vec()),
            (0x2, vec![0u8, 1, 254, 255]),
            (0x1, vec![b'x'; 200]),    // 16-bit length form
            (0x1, vec![b'y'; 70_000]), // 64-bit length form
        ] {
            let frame = encode_frame(op, &payload, [1, 2, 3, 4]);
            let raw = read_raw_frame(&mut &frame[..], MAX_MESSAGE).ok().unwrap();
            assert!(raw.fin);
            assert_eq!(raw.opcode, op);
            assert_eq!(raw.payload, payload);
        }
    }

    #[test]
    fn accept_value_matches_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn control_frame_rules() {
        // A fragmented (FIN=0) ping is a protocol error.
        let mut bad = encode_frame(0x9, b"p", [0, 0, 0, 0]);
        bad[0] &= 0x7f; // clear FIN
        assert!(matches!(
            read_raw_frame(&mut &bad[..], MAX_MESSAGE),
            Err(WsError::Protocol(1002, _))
        ));
        // Reserved bits fail (no extensions negotiated).
        let mut rsv = encode_frame(0x1, b"x", [0, 0, 0, 0]);
        rsv[0] |= 0x40;
        assert!(matches!(
            read_raw_frame(&mut &rsv[..], MAX_MESSAGE),
            Err(WsError::Protocol(1002, _))
        ));
    }
}
