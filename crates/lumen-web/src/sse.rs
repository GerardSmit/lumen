//! Server-Sent Events transport (WHATWG HTML §9.2) over HTTP or verified HTTPS — a streaming HTTP
//! GET whose `text/event-stream` body is delivered chunk-by-chunk to the native `EventSource` class
//! (`eventsource_class.rs`), which runs the line parser and reconnection logic. The fetch client fully buffers responses, so an
//! endless event stream can't ride on it; this is a dedicated streaming reader (the same
//! connect then reactor shape as the WebSocket client).
//!
//! ## How it runs on the loop
//! The dial, TLS handshake and response head run on one pool job that ends with the socket in
//! nonblocking mode. The body is then read on the loop thread whenever the readiness reactor
//! reports the socket readable: no thread and no timer wake while the stream is idle except one
//! inactivity timer (`READ_TIMEOUT`) that drops a stream which sends nothing at all.
//!
//! ## What's intentionally missing (v1)
//! - **CORS / credentials** — server runtime, no origin model (matches fetch here).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lumen_bind::NativeError;
use lumen_host::{Ctx, SpawnHandle, Value};
use lumen_os::reactor::Interest;

use crate::nbio::{self, Conn, Fill, HeadError, NbStream};
use crate::url;

const READ_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_HEADER_BYTES: usize = 64 << 10;
/// One delivered body chunk; the SSE parser reassembles across chunks.
const CHUNK: usize = 16 << 10;
const MAX_REDIRECTS: u8 = 5;

#[derive(Default)]
pub(crate) struct SseRegistry {
    next: u64,
    conns: HashMap<u64, SseEntry>,
}

struct SseEntry {
    /// `None` until the connect job has returned.
    conn: Option<Conn<Box<dyn NbStream>>>,
    dispatch: Value,
    /// `close()` ran before the connect job returned.
    dead: bool,
}

/// The connect task result: the open stream, or an error.
struct ConnectResult {
    id: u64,
    outcome: Result<Box<dyn NbStream>, ConnectError>,
}

enum ConnectError {
    /// A non-network failure that must NOT reconnect (bad scheme, wrong content-type, 204,
    /// a 4xx/5xx that isn't retriable).
    Fatal(String),
    /// A network-level failure the client may retry after its reconnection delay.
    Retriable(String),
}

fn sse_registry(ctx: &mut Ctx) -> &mut SseRegistry {
    ctx.host_mut::<SseRegistry>()
        .expect("web installs SseRegistry")
}

/// Open the stream for `target` on the pool and return its id. `dispatch(kind, ...)` then
/// receives `("open")`, `("chunk", u8array)`, `("fatal", message)` (no reconnect) or `("drop",
/// message)` (reconnect per the retry interval).
pub(crate) fn connect_stream(
    ctx: &mut Ctx,
    target: &str,
    last_event_id: &str,
    dispatch: Value,
) -> Result<u64, NativeError> {
    if !dispatch.is_callable() {
        return Err(NativeError::type_error("connect: dispatch must be a function"));
    }

    let u = url::parse(target, None).map_err(|e| NativeError::named("SyntaxError", e))?;
    match u.scheme.as_str() {
        "http" | "https" => {}
        other => {
            return Err(NativeError::named(
                "SyntaxError",
                format!("EventSource: unsupported scheme '{other}'"),
            ))
        }
    }

    let id = {
        let reg = sse_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.conns.insert(
            id,
            SseEntry {
                conn: None,
                dispatch: dispatch.clone(),
                dead: false,
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
    let target = target.to_string();
    let last_event_id = last_event_id.to_string();
    spawn.spawn_blocking(task, move || {
        Box::new(ConnectResult {
            id,
            outcome: open_stream(&target, &last_event_id),
        })
    });
    Ok(id)
}

/// Ends the stream at once: the socket is deregistered and closed, nothing more is dispatched.
pub(crate) fn close_stream(ctx: &mut Ctx, id: u64) {
    let reg = sse_registry(ctx);
    let Some(entry) = reg.conns.get_mut(&id) else {
        return;
    };
    match entry.conn.take() {
        Some(conn) => {
            let link = conn.link();
            reg.conns.remove(&id);
            link.cancel(ctx);
        }
        None => entry.dead = true,
    }
}

/// GET `target` with the SSE request headers, following redirects, and validate the response is
/// a `text/event-stream` 200 — returning the still-open stream positioned at the body start.
fn open_stream(target: &str, last_event_id: &str) -> Result<Box<dyn NbStream>, ConnectError> {
    let mut target = target.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let u = url::parse(&target, None).map_err(ConnectError::Fatal)?;
        if u.scheme != "http" && u.scheme != "https" {
            return Err(ConnectError::Fatal(format!(
                "unsupported scheme '{}'",
                u.scheme
            )));
        }
        let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
        let host = u.hostname().trim_matches(['[', ']']);
        let tcp = TcpStream::connect((host, port))
            .map_err(|e| ConnectError::Retriable(format!("connect: {e}")))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(nbio::HANDSHAKE_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(nbio::HANDSHAKE_TIMEOUT)).ok();

        let mut stream = if u.scheme == "https" {
            Stream::Tls(lumen_tls::TlsStream::connect(tcp, host).map_err(ConnectError::Retriable)?)
        } else {
            Stream::Plain(tcp)
        };
        let outcome = match &mut stream {
            Stream::Plain(s) => exchange(s, &u, last_event_id)?,
            Stream::Tls(s) => exchange(s, &u, last_event_id)?,
        };
        match outcome {
            Exchange::Open => {
                return stream
                    .into_nonblocking()
                    .map_err(|e| ConnectError::Retriable(format!("socket: {e}")))
            }
            Exchange::Redirect(next) => target = next,
        }
    }
    Err(ConnectError::Fatal("too many redirects".into()))
}

enum Stream {
    Plain(TcpStream),
    Tls(lumen_tls::TlsStream),
}

impl Stream {
    fn into_nonblocking(self) -> std::io::Result<Box<dyn NbStream>> {
        match self {
            Stream::Plain(s) => {
                s.set_nonblocking(true)?;
                Ok(Box::new(s))
            }
            Stream::Tls(mut s) => {
                s.set_nonblocking(true)?;
                Ok(Box::new(s))
            }
        }
    }
}

enum Exchange {
    Open,
    Redirect(String),
}

/// Sends the request and judges the response head.
fn exchange(
    stream: &mut (impl Read + Write),
    u: &url::Url,
    last_event_id: &str,
) -> Result<Exchange, ConnectError> {
    let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
    let host_header = if u.port.is_some() && u.port != Some(80) {
        format!("{}:{}", u.hostname(), port)
    } else {
        u.hostname().to_string()
    };
    let path = u.request_target();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept: text/event-stream\r\n\
         Cache-Control: no-cache\r\nConnection: keep-alive\r\n"
    );
    if !last_event_id.is_empty() {
        req.push_str(&format!("Last-Event-ID: {last_event_id}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| ConnectError::Retriable(format!("request write: {e}")))?;

    let head = nbio::read_head(stream, MAX_HEADER_BYTES).map_err(|e| match e {
        HeadError::TooLarge => ConnectError::Fatal("response head too large".into()),
        HeadError::Io(e) => ConnectError::Retriable(format!("head read: {e}")),
    })?;
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut location = None;
    let mut content_type = String::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("location") {
            location = Some(v.to_string());
        } else if k.eq_ignore_ascii_case("content-type") {
            content_type = v.to_ascii_lowercase();
        }
    }

    match status {
        200 => {
            if !content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
            {
                return Err(ConnectError::Fatal(format!(
                    "EventSource response Content-Type is '{content_type}', not text/event-stream"
                )));
            }
            Ok(Exchange::Open)
        }
        301 | 302 | 303 | 307 | 308 => {
            let Some(loc) = location else {
                return Err(ConnectError::Fatal("redirect without Location".into()));
            };
            Ok(Exchange::Redirect(
                url::parse(&loc, Some(&u.href()))
                    .map_err(ConnectError::Fatal)?
                    .href(),
            ))
        }
        // 204 No Content and 205 tell the client to STOP (no reconnect).
        204 | 205 => Err(ConnectError::Fatal(format!(
            "server returned {status} (stop)"
        ))),
        _ => Err(ConnectError::Fatal(format!(
            "server returned HTTP {status}"
        ))),
    }
}

fn decode_connect(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let ConnectResult { id, outcome } = *payload.downcast::<ConnectResult>().expect("sse payload");
    let kind = |name: &str, msg: String| {
        Ok(vec![
            Value::from_string(name.into()),
            Value::from_string(msg),
        ])
    };
    match outcome {
        Ok(stream) => {
            let dispatch = match sse_registry(ctx).conns.get(&id) {
                Some(e) if !e.dead => e.dispatch.clone(),
                _ => {
                    sse_registry(ctx).conns.remove(&id);
                    return kind("fatal", "closed".into());
                }
            };
            let mut conn = match Conn::open(ctx, stream, Interest::READ, id, dispatch, decode_read)
            {
                Ok(conn) => conn,
                Err(msg) => {
                    sse_registry(ctx).conns.remove(&id);
                    return kind("fatal", msg);
                }
            };
            conn.watch_reads(READ_TIMEOUT);
            // Body bytes may already sit in a TLS session or behind the head.
            conn.link().remote().raise(nbio::IO);
            if let Some(entry) = sse_registry(ctx).conns.get_mut(&id) {
                entry.conn = Some(conn);
            }
            Ok(vec![Value::from_string("open".into())])
        }
        Err(ConnectError::Fatal(msg)) => {
            sse_registry(ctx).conns.remove(&id);
            kind("fatal", msg)
        }
        Err(ConnectError::Retriable(msg)) => {
            sse_registry(ctx).conns.remove(&id);
            kind("drop", msg)
        }
    }
}

/// Settles one reactor wake: delivers up to one chunk, or ends the connection.
fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let id = *payload.downcast::<u64>().expect("sse payload");
    let reg = sse_registry(ctx);
    let Some(conn) = reg.conns.get_mut(&id).and_then(|e| e.conn.as_mut()) else {
        return Ok(Vec::new());
    };
    let link = conn.link();
    let flags = link.take();

    let mut end: Option<String> = None;
    let mut more = false;
    if flags & nbio::READ_STALL != 0 {
        if let Some(stalled) = conn.on_stall(flags) {
            end = Some(stalled.error().to_string());
        }
    }
    if end.is_none() {
        match conn.fill() {
            Ok(Fill::More) => more = true,
            Ok(Fill::Drained) => {}
            Ok(Fill::Eof) => {
                if conn.rx.is_empty() {
                    end = Some("stream ended".into());
                } else {
                    more = true;
                }
            }
            Err(e) => {
                if conn.rx.is_empty() {
                    end = Some(e.to_string());
                } else {
                    more = true;
                }
            }
        }
    }
    let chunk: Vec<u8> = {
        let bytes = conn.rx.bytes();
        let take = bytes.len().min(CHUNK);
        let chunk = bytes[..take].to_vec();
        conn.rx.consume(take);
        chunk
    };
    if let Some(msg) = end.filter(|_| chunk.is_empty()) {
        reg.conns.remove(&id);
        link.cancel(ctx);
        return Ok(vec![
            Value::from_string("drop".into()),
            Value::from_string(msg),
        ]);
    }
    if more || !conn.rx.is_empty() {
        link.mark(nbio::RESUME);
    }
    link.next_task(ctx);
    let conn = sse_registry(ctx)
        .conns
        .get_mut(&id)
        .and_then(|e| e.conn.as_mut())
        .expect("stream is still open");
    if let Err(e) = conn.rearm(true) {
        link.remote().raise(nbio::RESUME);
        if chunk.is_empty() {
            sse_registry(ctx).conns.remove(&id);
            link.cancel(ctx);
            return Ok(vec![
                Value::from_string("drop".into()),
                Value::from_string(e.to_string()),
            ]);
        }
    }
    if chunk.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![
        Value::from_string("chunk".into()),
        ctx.make_uint8array(&chunk)?,
    ])
}

// ---- test support -------------------------------------------------------------------------------

/// A minimal SSE server for this workspace's tests and benchmarks (NOT part of the runtime):
/// serves a canned `text/event-stream` body, honoring `Last-Event-ID` for the reconnect case.
#[doc(hidden)]
#[allow(dead_code)]
pub mod testing {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// What the SSE server should do.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        /// Serve three events (one named, one with an id) then close the stream.
        Events,
        /// First connection: one event with `id: 5`, then drop. Second connection (with
        /// Last-Event-ID: 5): serve a "resumed" event echoing the id, then close.
        Reconnect,
        /// Respond 200 with the WRONG content-type (text/plain) — a fatal error, no reconnect.
        WrongContentType,
        /// Respond 204 — tells the client to stop (no reconnect).
        NoContent,
    }

    /// Spawn the server; returns its port. Serves `conns` connections then exits.
    pub fn spawn(mode: Mode, conns: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut n = 0usize;
            for _ in 0..conns {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                serve_one(stream, mode, n);
                n += 1;
            }
        });
        port
    }

    fn serve_one(stream: std::net::TcpStream, mode: Mode, conn_index: usize) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if (&stream).read_exact(&mut byte).is_err() {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let last_event_id = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("last-event-id")
                .then(|| v.trim().to_string())
        });

        let write = |body: &str, ctype: &str, status: &str| {
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nCache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n{body}"
            );
            let _ = (&stream).write_all(resp.as_bytes());
        };

        match mode {
            Mode::WrongContentType => write("nope", "text/plain", "200 OK"),
            Mode::NoContent => {
                let _ =
                    (&stream).write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            }
            Mode::Events => write(
                "data: hello\n\nevent: tick\ndata: 42\n\nid: 9\ndata: line one\ndata: line two\n\n",
                "text/event-stream",
                "200 OK",
            ),
            Mode::Reconnect => {
                if conn_index == 0 {
                    // A short `retry:` so the reconnect (and the test) doesn't wait the default
                    // 3s; then one event with an id, then drop the connection (no clean close).
                    let _ = (&stream).write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                          Connection: close\r\n\r\nretry: 10\nid: 5\ndata: first\n\n",
                    );
                    // Close mid-stream by dropping `stream` here.
                } else {
                    let body = format!(
                        "data: resumed-from-{}\n\n",
                        last_event_id.unwrap_or_default()
                    );
                    write(&body, "text/event-stream", "200 OK");
                }
            }
        }
    }
}
