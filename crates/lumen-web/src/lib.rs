//! lumen-web — the WinterTC "Minimum Common Web Platform API", incrementally.
//!
//! Pure-JS pieces ship as `js_init` glue (see `src/js/`); Rust backs parsing, crypto, and the
//! network. Conformance checklist against the WinterTC minimum common API:
//!
//! - [x] `console`, timers, `queueMicrotask` (lumen-runtime/lumen-timers)
//! - [x] `DOMException`, `Event`, `CustomEvent`, `EventTarget`, `AbortController`,
//!   `AbortSignal` (incl. `abort()`/`timeout()` statics) — flat target, no capture phase
//! - [x] `TextEncoder` / `TextDecoder` (UTF-8 and Windows-1252 labels; `fatal` supported)
//! - [x] `atob` / `btoa`
//! - [x] `structuredClone` (objects/arrays/cycles, Date, RegExp, Map, Set, Error,
//!   ArrayBuffer, typed arrays; no transfer list)
//! - [x] `URL` / `URLSearchParams` (see url.rs for the parser's declared subset — no IDNA)
//! - [x] `performance.now()` (+`timeOrigin`), `navigator.userAgent`
//! - [x] `crypto.getRandomValues` / `crypto.randomUUID` (the OS CSPRNG via
//!   `lumen_os::proc::entropy`, no crates), `crypto.subtle.digest` (SHA-256 only)
//! - [x] `fetch` / `Headers` / `Request` / `Response` — HTTP and certificate-verified HTTPS
//!   through lumen-tls (system OpenSSL on Unix, rustls on Windows)
//! - [~] `Lumen.serve` — an HTTP/1.1 *server* (not a WinterTC API; follows the cross-runtime
//!   `serve((request) => Response)` convention of Deno/Bun/Workers). v1 is single-accept,
//!   `Connection: close`, buffered bodies, http only — see `server.rs` for what's deferred.
//! - [x] Streams (`ReadableStream`, `WritableStream`, `TransformStream`, the text and compression
//!   streams, queuing strategies) come from lumen-node's `webstreams.js` (Node's own WHATWG
//!   streams), which the runtime installs alongside this crate; the glue here only consumes them.
//!   Bodies remain buffered, so a stream used as a body must produce its data synchronously.
//! - [ ] `Blob` / `File` / `FormData`, `URLPattern`, `crypto.subtle` beyond digest, `WebSocket`

#[cfg(not(target_arch = "wasm32"))]
use lumen_host::SpawnHandle;
use lumen_bind::NativeError;
use lumen_host::{Ctx, Extension, OpError, OpState, Value};

#[cfg(not(target_arch = "wasm32"))]
mod http;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod sse;
mod url;
#[cfg(not(target_arch = "wasm32"))]
mod websocket;

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
use browser::{server, sse, websocket};

/// Close every HTTP listener the realm still holds, waking the accepts blocked on them.
pub fn close_servers(ctx: &mut Ctx) {
    #[cfg(not(target_arch = "wasm32"))]
    server::close_all(ctx);
    #[cfg(target_arch = "wasm32")]
    let _ = ctx;
}

/// WebSocket protocol internals — a from-scratch RFC 6455 codec. `websocket::testing` exposes a
/// minimal echo server other crates' tests and benchmarks drive the client against.
#[cfg(not(target_arch = "wasm32"))]
pub use websocket::testing as ws_testing;

/// SSE transport internals — `sse::testing` exposes a canned event-stream server for tests.
#[cfg(not(target_arch = "wasm32"))]
pub use sse::testing as sse_testing;
// The decoder parses the whole binary format; the MVP interpreter doesn't consume every field yet
// (reserved value-type data, mutability flags, etc.), and a few opcode matches read cleaner as
// explicit lists than ranges.
#[allow(dead_code, clippy::manual_range_patterns)]
mod wasm;
mod wasm_ops;

pub fn extension() -> Extension {
    Extension {
        name: "web",
        modules: &[
            lumen_host::namespace::<perf::Module>,
            lumen_host::namespace::<encoding::Module>,
            lumen_host::namespace::<url_ops::Module>,
            lumen_host::namespace::<crypto::Module>,
            lumen_host::namespace::<http_ops::Module>,
            lumen_host::namespace::<server::Module>,
            lumen_host::namespace::<websocket::Module>,
            lumen_host::namespace::<sse::Module>,
            lumen_host::namespace::<wasm_ops::WasmModule>,
        ],
        state_init: Some(|state: &mut OpState| {
            state.put(server::ServerRegistry::default());
            state.put(websocket::WsRegistry::default());
            state.put(sse::SseRegistry::default());
            state.put(wasm_ops::WasmStore::default());
        }),
        js_init: None,
        js_init_snapshot: Some(JS_GLUE_AOT),
    }
}

/// One IIFE (preamble captures and deletes the raw `__*` namespaces, the rest defines the
/// standard classes over them), assembled by `build.rs` from `src/js/*.js` — the single source
/// of truth — and precompiled there to an ahead-of-time blob (AST, bytecode, compressed function
/// text), loaded at boot (see `lumen_host::install`).
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web_glue.aot"));

#[lumen_bind::module(name = "__perf")]
mod perf {
    #[op]
    pub fn now() -> f64 {
        lumen_host::perf::now_ms()
    }

    /// `performance.timeOrigin`: Unix-epoch milliseconds at the monotonic clock's zero point.
    #[op(name = "timeOrigin")]
    pub fn time_origin() -> f64 {
        lumen_host::perf::time_origin_ms()
    }
}

#[lumen_bind::module(name = "__encoding")]
mod encoding {
    use super::*;

    #[op(coerce)]
    pub fn encode(s: String) -> Vec<u8> {
        lumen_host::well_formed_utf8(&s).as_bytes().to_vec()
    }

    /// `(u8array, fatal)`; the glue has already converted ArrayBuffer inputs to views.
    #[op(coerce)]
    pub fn decode(bytes: &[u8], fatal: bool) -> Result<Value, OpError> {
        let text = if fatal {
            std::str::from_utf8(bytes)
                .map_err(|_| OpError::type_error("TextDecoder: invalid utf-8 (fatal)"))?
                .to_owned()
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(text)))
    }

    /// Base64 of a Latin-1 string, or `null` when a char is past U+00FF.
    #[op(coerce)]
    pub fn btoa(s: String) -> Option<String> {
        let bytes = s.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect::<Option<Vec<u8>>>()?;
        Some(lumen_common::codec::base64_encode(&bytes, false, true))
    }

    /// forgiving-base64 decode to a Latin-1 string, or `null` on invalid input.
    #[op(coerce)]
    pub fn atob(s: String) -> Option<String> {
        lumen_common::codec::base64_decode_forgiving(s.as_bytes())
            .map(|bytes| bytes.into_iter().map(char::from).collect())
    }
}

/// `[href, protocol_end, username_end, host_start, host_end, port, pathname_start, search_start,
/// hash_start, scheme_type]`: the shape Node's `URLContext` keeps (ada's url_components).
fn url_record(ctx: &mut Ctx, u: &url::Url) -> Value {
    let mut items = vec![Value::from_string(u.href())];
    items.extend(u.components().iter().map(|&c| Value::Num(c as f64)));
    ctx.make_array(items)
}

#[lumen_bind::module(name = "__url")]
mod url_ops {
    use super::*;

    /// `(input, base?)` -> URL record array, or `null` when either fails to parse (the JS side
    /// raises `ERR_INVALID_URL`).
    #[op(coerce)]
    pub fn parse(ctx: &mut Ctx, input: String, base: Option<String>) -> Option<Value> {
        let base = match base {
            Some(b) => Some(url::parse_url(&b, None)?),
            None => None,
        };
        url::parse_url(&input, base.as_ref()).map(|u| url_record(ctx, &u))
    }

    /// `(href, action, value)` -> updated record, or `null` when the setter declines (Node's
    /// `bindingUrl.update`; action numbering is internal/url's `updateActions`).
    #[op(coerce)]
    pub fn update(ctx: &mut Ctx, href: String, action: i32, value: String) -> Option<Value> {
        let mut u = url::parse_url(&href, None)?;
        let ok = match action {
            0 => u.set_protocol(&value),
            1 => u.set_host(&value),
            2 => u.set_hostname(&value),
            3 => u.set_port(&value),
            4 => u.set_username(&value),
            5 => u.set_password(&value),
            6 => u.set_pathname(&value),
            7 => {
                u.set_search(&value);
                true
            }
            8 => {
                u.set_hash(&value);
                true
            }
            9 => u.set_href(&value),
            _ => false,
        };
        ok.then(|| url_record(ctx, &u))
    }

    #[op(coerce, name = "canParse")]
    pub fn can_parse(input: String, base: Option<String>) -> bool {
        match base {
            Some(b) => url::parse_url(&b, None).is_some_and(|base| url::parse_url(&input, Some(&base)).is_some()),
            None => url::parse_url(&input, None).is_some(),
        }
    }

    #[op(coerce, name = "domainToASCII")]
    pub fn domain_to_ascii(input: String) -> String {
        url::domain_to_ascii(&input)
    }

    #[op(coerce, name = "domainToUnicode")]
    pub fn domain_to_unicode(input: String) -> String {
        url::domain_to_unicode(&input)
    }

    #[op(coerce, name = "toASCII")]
    pub fn idna_to_ascii(input: String) -> String {
        url::idna_to_ascii(&input)
    }

    #[op(coerce, name = "toUnicode")]
    pub fn idna_to_unicode(input: String) -> String {
        url::domain_to_unicode_raw(&input)
    }

    /// `(href, hash, unicode, search, auth)` -> href with the dropped parts removed.
    #[op(coerce)]
    pub fn format(href: String, hash: bool, unicode: bool, search: bool, auth: bool) -> String {
        url::format(&href, hash, unicode, search, auth).unwrap_or(href)
    }
}

// ---- crypto ----

fn entropy(buf: &mut [u8]) -> Result<(), OpError> {
    lumen_os::proc::entropy(buf).map_err(|e| OpError::error(format!("no randomness source: {e}")))
}

/// `n` cryptographically-random bytes (the WebSocket handshake key needs these, same source as
/// `crypto.getRandomValues`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn web_random_bytes(n: usize) -> Result<Vec<u8>, OpError> {
    let mut buf = vec![0u8; n];
    entropy(&mut buf)?;
    Ok(buf)
}

#[lumen_bind::module(name = "__crypto")]
mod crypto {
    use super::*;

    /// Fill the given typed array in place (the glue enforces the 65536-byte quota + returns it).
    #[op]
    pub fn fill(buf: &mut [u8]) -> Result<(), OpError> {
        entropy(buf)
    }

    #[op]
    pub fn uuid() -> Result<String, OpError> {
        let mut b = [0u8; 16];
        entropy(&mut b)?;
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // variant 10
        let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
        Ok(format!("{}-{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32]))
    }

    #[op(coerce)]
    pub fn digest(name: String, data: &[u8]) -> Result<Vec<u8>, OpError> {
        use lumen_common::hash::{digest, Algo};
        let algo = match name.as_str() {
            "SHA-1" => Algo::Sha1,
            "SHA-256" => Algo::Sha256,
            "SHA-384" => Algo::Sha384,
            "SHA-512" => Algo::Sha512,
            _ => return Err(OpError::type_error(format!("unsupported digest {name}"))),
        };
        Ok(digest(algo, data).to_vec())
    }
}

// ---- fetch ----

#[cfg(target_arch = "wasm32")]
use browser::http_ops;

/// The `(resolve, reject)` callback pair an async op settles through, or a `TypeError`.
pub(crate) fn callbacks(resolve: Value, reject: Value, who: &str) -> Result<(Value, Value), NativeError> {
    if resolve.is_callable() && reject.is_callable() {
        Ok((resolve, reject))
    } else {
        Err(NativeError::type_error(format!("{who} expects (resolve, reject)")))
    }
}

/// A request body: bytes of a typed array, or the UTF-8 of any other value; `null` and `undefined`
/// are no body.
pub(crate) fn read_body(ctx: &mut Ctx, v: &Value) -> Result<Option<Vec<u8>>, Value> {
    if matches!(v, Value::Undefined | Value::Null) {
        return Ok(None);
    }
    Ok(Some(match ctx.typed_array_bytes(v) {
        Some(bytes) => bytes,
        None => ctx.coerce_string(v)?.as_bytes().to_vec(),
    }))
}

#[cfg(not(target_arch = "wasm32"))]
#[lumen_bind::module(name = "__http")]
mod http_ops {
    use super::*;

    /// `(method, url, headerPairs, bodyOrUndefined, resolve, reject)`: one HTTP request on the
    /// threadpool, settled through the TaskRegistry like every async op.
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
        let headers = read_header_pairs(ctx, &headers)?;
        let body = read_body(ctx, &body)?;
        let (resolve, reject) = callbacks(resolve, reject, "__http.request")?;
        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_http);
        let spawn = ctx
            .op_state()
            .get::<SpawnHandle>()
            .expect("runtime installs the spawn handle")
            .clone();
        spawn.spawn_blocking(id, move || {
            Box::new(http::request(&method, &target, &headers, body.as_deref()))
        });
        Ok(())
    }
}

/// A JS `[[k, v], ...]` array into Rust pairs, via the curated member API.
pub(crate) fn read_header_pairs(ctx: &mut Ctx, v: &Value) -> Result<Vec<(String, String)>, Value> {
    let mut out = Vec::new();
    if v.as_obj().is_none() {
        return Ok(out);
    }
    let len = ctx
        .get_member(v, "length")
        .map_err(|_| ctx.make_error("TypeError", "__http.request: headers must be an array"))?;
    let Value::Num(len) = len else {
        return Ok(out);
    };
    for i in 0..(len as usize) {
        let pair = ctx
            .get_member(v, &i.to_string())
            .unwrap_or(Value::Undefined);
        let k = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
        let val = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
        out.push((
            ctx.coerce_string(&k)?.to_string(),
            ctx.coerce_string(&val)?.to_string(),
        ));
    }
    Ok(out)
}

/// Build the raw-response object the JS glue wraps into a `Response`.
#[cfg(not(target_arch = "wasm32"))]
fn decode_http(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let result = *payload
        .downcast::<Result<http::HttpResponse, String>>()
        .expect("http payload");
    let response = match result {
        Ok(r) => r,
        Err(message) => return Err(ctx.make_error("TypeError", message)),
    };
    let obj = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&obj, "status", Value::Num(response.status as f64));
    let _ = ctx.set_member(&obj, "statusText", Value::from_string(response.status_text));
    let _ = ctx.set_member(&obj, "url", Value::from_string(response.url));
    let pairs: Vec<Value> = response
        .headers
        .into_iter()
        .map(|(k, v)| ctx.make_array(vec![Value::from_string(k), Value::from_string(v)]))
        .collect();
    let headers = ctx.make_array(pairs);
    let _ = ctx.set_member(&obj, "headers", headers);
    let body = ctx.make_uint8array(&response.body)?;
    let _ = ctx.set_member(&obj, "body", body);
    Ok(vec![obj])
}
