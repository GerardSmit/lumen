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
//!   Fetch reads native response bodies incrementally; body consumption and cloning also
//!   accept asynchronous streams. Request uploads are prepared before transport delivery.
//! - [ ] `Blob` / `File` / `FormData`, `URLPattern`, `crypto.subtle` beyond digest, `WebSocket`

use lumen_bind::NativeError;
#[cfg(not(target_arch = "wasm32"))]
use lumen_host::SpawnHandle;
use lumen_host::{Ctx, Extension, OpError, OpState, Value};

#[cfg(not(target_arch = "wasm32"))]
mod http;
#[cfg(not(target_arch = "wasm32"))]
mod http_body;
#[cfg(target_arch = "wasm32")]
#[path = "browser_body.rs"]
mod http_body;
mod request_control;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod sse;
mod url;
#[cfg(not(target_arch = "wasm32"))]
mod websocket;

/// A fetched web resource with the URL that survived HTTP redirects. Runtime module loaders use
/// this instead of maintaining a second HTTP/TLS implementation.
#[cfg(not(target_arch = "wasm32"))]
pub struct ModuleResource {
    pub url: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// A bounded native resource response, including unsuccessful HTTP status
/// codes. It uses the same transport, redirect, routing and trust handling as
/// module loading; callers apply their resource type's response policy.
#[cfg(not(target_arch = "wasm32"))]
pub use http::HttpResponse as ResourceResponse;

/// Per-runtime routing and trust configuration for native HTTP(S) fetches.
///
/// A route changes only the socket address. Requests retain the URL's logical host and port for
/// the HTTP `Host` header, TLS SNI, and certificate hostname verification. Clones share an
/// immutable snapshot; mutating a clone uses copy-on-write, so an in-flight request keeps the
/// configuration it started with.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
pub struct FetchConfig {
    routes: std::sync::Arc<std::collections::HashMap<(String, u16), FetchRoute>>,
    require_routes: bool,
    /// Internal single-hop policy for the Fetch origin adapter.
    manual_redirect: bool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(crate) struct FetchRoute {
    pub(crate) address: std::net::SocketAddr,
    pub(crate) extra_ca_pem: Option<std::sync::Arc<[u8]>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FetchConfig {
    /// Restrict requests, including redirected requests, to explicitly configured routes.
    /// Ordinary runtimes allow system DNS by default; fixture runtimes opt into this policy.
    pub fn set_require_routes(&mut self, required: bool) {
        self.require_routes = required;
    }

    pub(crate) fn requires_routes(&self) -> bool {
        self.require_routes
    }

    /// Route a logical URL host and port to a concrete socket address.
    pub fn set_route(
        &mut self,
        host: &str,
        logical_port: u16,
        address: std::net::SocketAddr,
    ) -> Result<(), String> {
        self.set_route_inner(host, logical_port, address, None)
    }

    /// Route a logical URL host and port and add the supplied PEM certificates to the existing
    /// system trust roots for HTTPS requests through this route.
    pub fn set_route_with_extra_roots(
        &mut self,
        host: &str,
        logical_port: u16,
        address: std::net::SocketAddr,
        extra_ca_pem: impl Into<Vec<u8>>,
    ) -> Result<(), String> {
        let extra_ca_pem = extra_ca_pem.into();
        if extra_ca_pem.is_empty() {
            return Err("extra CA PEM bundle is empty".into());
        }
        self.set_route_inner(host, logical_port, address, Some(extra_ca_pem.into()))
    }

    /// Remove a route, returning whether one was present.
    pub fn remove_route(&mut self, host: &str, logical_port: u16) -> Result<bool, String> {
        let host = normalize_route_host(host)?;
        Ok(std::sync::Arc::make_mut(&mut self.routes)
            .remove(&(host, logical_port))
            .is_some())
    }

    fn set_route_inner(
        &mut self,
        host: &str,
        logical_port: u16,
        address: std::net::SocketAddr,
        extra_ca_pem: Option<std::sync::Arc<[u8]>>,
    ) -> Result<(), String> {
        if logical_port == 0 {
            return Err("logical route port must be nonzero".into());
        }
        let host = normalize_route_host(host)?;
        std::sync::Arc::make_mut(&mut self.routes).insert(
            (host, logical_port),
            FetchRoute {
                address,
                extra_ca_pem,
            },
        );
        Ok(())
    }

    pub(crate) fn route_for(&self, host: &str, logical_port: u16) -> Option<FetchRoute> {
        let host = normalize_route_host(host).ok()?;
        self.routes.get(&(host, logical_port)).cloned()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn normalize_route_host(host: &str) -> Result<String, String> {
    let trimmed = host.trim();
    let (host, was_bracketed) = match trimmed
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        Some(host) => (host, true),
        None => (trimmed, false),
    };
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return Ok(address.to_string());
    }
    if was_bracketed || host.contains(':') {
        return Err(format!("invalid route host '{trimmed}'"));
    }

    // Reuse Lumen's WHATWG URL host and IDNA normalization rather than maintaining another
    // hostname parser in the network adapter.
    let parsed = crate::url::parse(&format!("http://{host}/"), None)?;
    if parsed.host.is_none()
        || parsed.port.is_some()
        || parsed.path != "/"
        || parsed.query.is_some()
        || parsed.fragment.is_some()
        || !parsed.username.is_empty()
        || !parsed.password.is_empty()
    {
        return Err(format!("invalid route host '{trimmed}'"));
    }
    let normalized = parsed.hostname().trim_end_matches('.').to_owned();
    if normalized.is_empty() {
        return Err("route host is empty".into());
    }
    Ok(normalized)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod fetch_config_tests {
    use super::FetchConfig;
    use std::net::SocketAddr;

    #[test]
    fn cloned_fetch_config_keeps_its_route_snapshot() {
        let first: SocketAddr = "127.0.0.1:8001".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:8002".parse().unwrap();
        let mut original = FetchConfig::default();
        original.set_route("WPT.TEST.", 8000, first).unwrap();
        let snapshot = original.clone();
        original.set_route("wpt.test", 8000, second).unwrap();

        assert_eq!(snapshot.route_for("wpt.test", 8000).unwrap().address, first);
        assert_eq!(
            original.route_for("WPT.TEST.", 8000).unwrap().address,
            second
        );
        assert!(original.remove_route("wpt.test", 8000).unwrap());
        assert!(original.route_for("wpt.test", 8000).is_none());
        assert_eq!(snapshot.route_for("wpt.test", 8000).unwrap().address, first);
    }
}

/// Fetch a complete HTTP(S) resource using the same verified transport and redirect handling as
/// `fetch()`. This synchronous form is intended for the runtime's synchronous ESM loader, which
/// runs on a worker's own runtime thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_module_resource(url: &str) -> Result<ModuleResource, String> {
    let response = http::request("GET", url, &[], None)?;
    module_resource_from_response(response)
}

/// Fetch a complete HTTP(S) module resource using the supplied routing and trust snapshot.
/// Runtime module loaders can use this when they have per-runtime fetch configuration.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_module_resource_with_config(
    url: &str,
    config: &FetchConfig,
) -> Result<ModuleResource, String> {
    let response = load_resource_with_config(url, config)?;
    module_resource_from_response(response)
}

/// Fetch a resource with a captured routing/trust configuration, retaining
/// HTTP failures for callers which model stylesheet or image load failure.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_resource_with_config(
    url: &str,
    config: &FetchConfig,
) -> Result<ResourceResponse, String> {
    http::request_with_config("GET", url, &[], None, config)
}

/// Fetch a module resource using the supplied routing and trust snapshot, requiring the initial
/// URL and every redirect target to have the same HTTP(S) origin. This is used for classic Worker
/// entry scripts, whose fetch uses same-origin mode; `importScripts()` continues to use the
/// cross-origin-capable `load_module_resource_with_config` path.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_module_resource_same_origin_with_config(
    url: &str,
    config: &FetchConfig,
) -> Result<ModuleResource, String> {
    let response = http::request_with_config_same_origin("GET", url, &[], None, config)?;
    module_resource_from_response(response)
}

#[cfg(not(target_arch = "wasm32"))]
fn module_resource_from_response(response: http::HttpResponse) -> Result<ModuleResource, String> {
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "module fetch '{}' failed with HTTP {}",
            response.url, response.status
        ));
    }
    let content_type = response.content_type();
    Ok(ModuleResource {
        url: response.url,
        content_type,
        bytes: response.body,
    })
}

/// Whether a response has one of the JavaScript MIME essences accepted for module scripts.
pub use lumen_common::mime::is_javascript_module_mime;

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
            encoding_namespace,
            url_namespace,
            lumen_host::namespace::<crypto::Module>,
            lumen_host::namespace::<http_ops::Module>,
            lumen_host::namespace::<server::Module>,
            lumen_host::namespace::<websocket::Module>,
            lumen_host::namespace::<sse::Module>,
            lumen_host::namespace::<wasm_ops::WasmModule>,
        ],
        state_init: Some(|state: &mut OpState| {
            #[cfg(not(target_arch = "wasm32"))]
            {
                state.put(FetchConfig::default());
                state.put(server::ServerRegistry::default());
                state.put(websocket::WsRegistry::default());
                state.put(sse::SseRegistry::default());
            }
            state.put(wasm_ops::WasmStore::default());
        }),
        js_init: {
            #[cfg(feature = "compiler")]
            {
                Some(JS_GLUE_SOURCE)
            }
            #[cfg(not(feature = "compiler"))]
            {
                None
            }
        },
        js_init_snapshot: Some(JS_GLUE_AOT),
    }
}

/// One IIFE (preamble captures and deletes the raw `__*` namespaces, the rest defines the
/// standard classes over them), assembled by `build.rs` from `src/js/*.js` — the single source
/// of truth — and precompiled there to an ahead-of-time blob (AST, bytecode, compressed function
/// text), loaded at boot (see `lumen_host::install`).
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web_glue.aot"));
#[cfg(feature = "compiler")]
const JS_GLUE_SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/web_glue.js"));

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

fn encoding_namespace(ctx: &mut Ctx) -> Result<(), Value> {
    let ns = ctx.namespace_object("__encoding");
    ctx.install_module::<lumen_host::encoding::bindings::Module>(&ns)
}

fn url_namespace(ctx: &mut Ctx) -> Result<(), Value> {
    let ns = ctx.namespace_object("__url");
    ctx.install_module::<lumen_host::url::bindings::Module>(&ns)
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
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &s[0..8],
            &s[8..12],
            &s[12..16],
            &s[16..20],
            &s[20..32]
        ))
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
pub(crate) fn callbacks(
    resolve: Value,
    reject: Value,
    who: &str,
) -> Result<(Value, Value), NativeError> {
    if resolve.is_callable() && reject.is_callable() {
        Ok((resolve, reject))
    } else {
        Err(NativeError::type_error(format!(
            "{who} expects (resolve, reject)"
        )))
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

    /// `(method, url, headerPairs, bodyOrUndefined, resolve, reject, redirectMode?)`: one HTTP
    /// request on the threadpool, settled through the TaskRegistry like every async op. Returns
    /// the request's cancellation control.
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
        redirect_mode: Option<String>,
    ) -> Result<Value, OpError> {
        let headers = read_header_pairs(ctx, &headers)?;
        let body = read_body(ctx, &body)?;
        let (resolve, reject) = callbacks(resolve, reject, "__http.request")?;
        let mut fetch_config = ctx
            .op_state()
            .get::<FetchConfig>()
            .cloned()
            .unwrap_or_default();
        if let Some(mode) = redirect_mode {
            if mode != "follow" && mode != "manual" {
                return Err(OpError::type_error("invalid transport redirect mode"));
            }
            fetch_config.manual_redirect = mode == "manual";
        }
        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_http);
        let cancellation = lumen_os::net::TcpCancellation::default();
        let worker_cancellation = cancellation.clone();
        let spawn = ctx
            .op_state()
            .get::<SpawnHandle>()
            .expect("runtime installs the spawn handle")
            .clone();
        spawn.spawn_blocking(id, move || {
            Box::new(http::open_request_cancellable_with_config(
                &method,
                &target,
                &headers,
                body.as_deref(),
                &worker_cancellation,
                &fetch_config,
            ))
        });
        Ok(ctx.new_instance(request_control::RequestControl { id, cancellation }))
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
        .downcast::<Result<http::OpenHttpResponse, String>>()
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
    if response.body.is_empty() {
        let _ = ctx.set_member(&obj, "body", Value::Null);
    } else {
        let body = ctx.new_instance(http_body::ResponseBody::new(response.body));
        let _ = ctx.set_member(&obj, "bodyReader", body);
    }
    Ok(vec![obj])
}
