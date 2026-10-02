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
//!   `lumen_host::fill_random`, no crates), `crypto.subtle.digest` (SHA-256 only)
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
use lumen_host::{ops, Ctx, Extension, OpState, Value};

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
use browser as server;
#[cfg(target_arch = "wasm32")]
use browser as sse;
#[cfg(target_arch = "wasm32")]
use browser as websocket;

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
        modules: &[],
        globals: &[],
        namespaces: &[
            (
                "__perf",
                ops!["now" (0) => op_perf_now, "timeOrigin" (0) => op_time_origin],
            ),
            (
                "__encoding",
                ops![
                    "encode" (1) => op_encode,
                    "decode" (2) => op_decode,
                    "btoa" (1) => op_btoa,
                    "atob" (1) => op_atob,
                ],
            ),
            (
                "__url",
                ops![
                    "parse" (2) => op_url_parse,
                    "update" (3) => op_url_update,
                    "canParse" (2) => op_url_can_parse,
                    "domainToASCII" (1) => op_url_domain_to_ascii,
                    "domainToUnicode" (1) => op_url_domain_to_unicode,
                    "toASCII" (1) => op_idna_to_ascii,
                    "toUnicode" (1) => op_idna_to_unicode,
                    "format" (5) => op_url_format,
                ],
            ),
            ("__http", ops!["request" (6) => http_request]),
            (
                "__http_server",
                ops![
                    "listen" (3) => server::op_server_listen,
                    "respond" (7) => server::op_server_respond,
                    "close" (1) => server::op_server_close,
                    "version" (0) => server::op_server_version,
                ],
            ),
            (
                "__crypto",
                ops![
                    "fill" (1) => op_random_fill,
                    "uuid" (0) => op_uuid,
                    "digest" (2) => op_digest,
                ],
            ),
            (
                "__ws",
                ops![
                    "connect" (3) => websocket::op_ws_connect,
                    "send" (2) => websocket::op_ws_send,
                    "close" (3) => websocket::op_ws_close,
                    "upgrade" (5) => websocket::op_ws_upgrade,
                ],
            ),
            (
                "__sse",
                ops![
                    "connect" (3) => sse::op_sse_connect,
                    "close" (1) => sse::op_sse_close,
                ],
            ),
            (
                "__wasm",
                ops![
                    "validate" (1) => wasm_ops::op_validate,
                    "compile" (1) => wasm_ops::op_compile,
                    "moduleExports" (1) => wasm_ops::op_module_exports,
                    "moduleImports" (1) => wasm_ops::op_module_imports,
                    "allocMemory" (2) => wasm_ops::op_alloc_memory,
                    "allocTable" (2) => wasm_ops::op_alloc_table,
                    "allocGlobal" (3) => wasm_ops::op_alloc_global,
                    "instantiate" (2) => wasm_ops::op_instantiate,
                    "call" (2) => wasm_ops::op_call,
                    "func" (1) => wasm_ops::op_func,
                    "setErrors" (1) => wasm_ops::op_set_errors,
                    "memBuffer" (1) => wasm_ops::op_mem_buffer,
                    "memGrow" (2) => wasm_ops::op_mem_grow,
                    "tableGet" (2) => wasm_ops::op_table_get,
                    "tableSet" (3) => wasm_ops::op_table_set,
                    "tableSize" (1) => wasm_ops::op_table_size,
                    "globalGet" (1) => wasm_ops::op_global_get,
                    "globalSet" (2) => wasm_ops::op_global_set,
                ],
            ),
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

fn op_perf_now(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(lumen_host::perf::now_ms()))
}

/// `performance.timeOrigin`: Unix-epoch milliseconds at the monotonic clock's zero point.
fn op_time_origin(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(lumen_host::perf::time_origin_ms()))
}

// ---- encoding ----

fn op_encode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let s = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    let bytes = lumen_host::well_formed_utf8(&s).as_bytes().to_vec();
    ctx.make_uint8array(&bytes)
}

/// `(u8array, fatal)`; the glue has already converted ArrayBuffer inputs to views.
fn op_decode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "TextDecoder.decode expects a BufferSource"));
    };
    let fatal = matches!(args.get(1), Some(Value::Bool(true)));
    let text = if fatal {
        match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Err(ctx.make_error("TypeError", "TextDecoder: invalid utf-8 (fatal)")),
        }
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(text)))
}

/// Base64 of a Latin-1 string, or `null` when a char is past U+00FF.
fn op_btoa(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let s = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    let mut bytes = Vec::with_capacity(s.len());
    for c in s.chars() {
        match u8::try_from(u32::from(c)) {
            Ok(b) => bytes.push(b),
            Err(_) => return Ok(Value::Null),
        }
    }
    Ok(Value::from_string(lumen_common::codec::base64_encode(&bytes, false, true)))
}

/// forgiving-base64 decode to a Latin-1 string, or `null` on invalid input.
fn op_atob(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let s = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    Ok(match lumen_common::codec::base64_decode_forgiving(s.as_bytes()) {
        Some(bytes) => Value::from_string(bytes.into_iter().map(char::from).collect::<String>()),
        None => Value::Null,
    })
}

// ---- url ----

fn str_arg(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<Option<String>, Value> {
    match args.get(i) {
        None | Some(Value::Undefined) => Ok(None),
        Some(v) => Ok(Some(ctx.coerce_string(v)?.to_string())),
    }
}

/// `[href, protocol_end, username_end, host_start, host_end, port, pathname_start, search_start,
/// hash_start, scheme_type]` — the shape Node's `URLContext` keeps (ada's url_components).
fn url_record(ctx: &mut Ctx, u: &url::Url) -> Value {
    let mut items = vec![Value::from_string(u.href())];
    items.extend(u.components().iter().map(|&c| Value::Num(c as f64)));
    ctx.make_array(items)
}

/// `(input, base?)` -> URL record array, or `null` when either fails to parse (the JS side raises
/// `ERR_INVALID_URL`).
fn op_url_parse(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    let base = match str_arg(ctx, args, 1)? {
        Some(b) => match url::parse_url(&b, None) {
            Some(u) => Some(u),
            None => return Ok(Value::Null),
        },
        None => None,
    };
    Ok(match url::parse_url(&input, base.as_ref()) {
        Some(u) => url_record(ctx, &u),
        None => Value::Null,
    })
}

/// `(href, action, value)` -> updated record, or `null` when the setter declines (Node's
/// `bindingUrl.update`; action numbering is internal/url's `updateActions`).
fn op_url_update(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let href = str_arg(ctx, args, 0)?.unwrap_or_default();
    let action = match args.get(1) {
        Some(Value::Num(n)) => *n as i32,
        _ => -1,
    };
    let value = str_arg(ctx, args, 2)?.unwrap_or_default();
    let Some(mut u) = url::parse_url(&href, None) else {
        return Ok(Value::Null);
    };
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
    Ok(if ok { url_record(ctx, &u) } else { Value::Null })
}

fn op_url_can_parse(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    let ok = match str_arg(ctx, args, 1)? {
        Some(b) => url::parse_url(&b, None).is_some_and(|base| url::parse_url(&input, Some(&base)).is_some()),
        None => url::parse_url(&input, None).is_some(),
    };
    Ok(Value::Bool(ok))
}

fn op_url_domain_to_ascii(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    Ok(Value::from_string(url::domain_to_ascii(&input)))
}

fn op_url_domain_to_unicode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    Ok(Value::from_string(url::domain_to_unicode(&input)))
}

fn op_idna_to_ascii(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    Ok(Value::from_string(url::idna_to_ascii(&input)))
}

fn op_idna_to_unicode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = str_arg(ctx, args, 0)?.unwrap_or_default();
    Ok(Value::from_string(url::domain_to_unicode_raw(&input)))
}

/// `(href, hash, unicode, search, auth)` -> href with the dropped parts removed.
fn op_url_format(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let href = str_arg(ctx, args, 0)?.unwrap_or_default();
    let flag = |i: usize| matches!(args.get(i), Some(Value::Bool(true)));
    Ok(match url::format(&href, flag(1), flag(2), flag(3), flag(4)) {
        Some(s) => Value::from_string(s),
        None => Value::from_string(href),
    })
}

// ---- crypto ----

/// `n` cryptographically-random bytes (the WebSocket handshake key needs these, same source as
/// `crypto.getRandomValues`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn web_random_bytes(ctx: &mut Ctx, n: usize) -> Result<Vec<u8>, Value> {
    random_bytes(ctx, n)
}

fn random_bytes(ctx: &mut Ctx, n: usize) -> Result<Vec<u8>, Value> {
    let mut buf = vec![0u8; n];
    lumen_host::fill_random(&mut buf)
        .map_err(|e| ctx.make_error("Error", format!("no randomness source: {e}")))?;
    Ok(buf)
}

/// Fill the given typed array in place (the glue enforces the 65536-byte quota + returns it).
fn op_random_fill(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let Some(existing) = ctx.typed_array_bytes(&v) else {
        return Err(ctx.make_error("TypeError", "getRandomValues expects a typed array"));
    };
    let bytes = random_bytes(ctx, existing.len())?;
    ctx.typed_array_set_bytes(&v, &bytes);
    Ok(Value::Undefined)
}

fn op_uuid(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let mut b = random_bytes(ctx, 16)?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    let s = h.join("");
    Ok(Value::from_string(format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )))
}

fn op_digest(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use lumen_common::hash::{digest, Algo};
    let name = str_arg(ctx, args, 0)?.unwrap_or_default();
    let v = args.get(1).unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "digest expects a BufferSource"));
    };
    let digest = match name.as_str() {
        "SHA-1" => digest(Algo::Sha1, &bytes),
        "SHA-256" => digest(Algo::Sha256, &bytes),
        "SHA-384" => digest(Algo::Sha384, &bytes),
        "SHA-512" => digest(Algo::Sha512, &bytes),
        _ => return Err(ctx.make_error("TypeError", format!("unsupported digest {name}"))),
    };
    ctx.make_uint8array(&digest)
}

// ---- fetch ----

#[cfg(target_arch = "wasm32")]
use browser::op_http_request as http_request;
#[cfg(not(target_arch = "wasm32"))]
use op_http_request as http_request;

/// `(method, url, headerPairs, bodyOrUndefined, resolve, reject)`: one HTTP request on the
/// threadpool, settled through the TaskRegistry like every async op.
#[cfg(not(target_arch = "wasm32"))]
fn op_http_request(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let method = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let target = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let headers = read_header_pairs(ctx, args.get(2).unwrap_or(&Value::Undefined))?;
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
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_http);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(id, move || {
        Box::new(http::request(&method, &target, &headers, body.as_deref()))
    });
    Ok(Value::Undefined)
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
