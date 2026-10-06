//! lumen-web — the WinterTC "Minimum Common Web Platform API", incrementally.
//!
//! Pure-JS pieces ship as `js_init` glue (see `src/js/`); Rust backs parsing, crypto, and the
//! network. Conformance checklist against the WinterTC minimum common API:
//!
//! - [x] `console`, timers, `queueMicrotask` (lumen-runtime/lumen-timers)
//! - [x] `DOMException`, `Event`, `CustomEvent`, `EventTarget`, `AbortController`,
//!   `AbortSignal` (incl. `abort()`/`timeout()` statics) — flat target, no capture phase
//! - [x] `TextEncoder` / `TextDecoder` (every WHATWG label; `fatal`, `ignoreBOM`, `stream`) and
//!   `atob` / `btoa`: native classes and ops in `lumen_host::encoding`, published lazily
//! - [x] `structuredClone` (objects/arrays/cycles, Date, RegExp, Map, Set, Error, Blob/File,
//!   ArrayBuffer and views, `SharedArrayBuffer`, transferable buffers, ports and signals): native
//!   in `lumen_host::structured_clone`, published lazily
//! - [x] `URL` / `URLSearchParams`: native classes in `lumen_host::url` over the WHATWG parser in
//!   `lumen_common::url`, published lazily
//! - [x] `performance.now()` (+`timeOrigin`), `navigator.userAgent`
//! - [x] `crypto.getRandomValues` / `crypto.randomUUID` (the OS CSPRNG via
//!   `lumen_os::proc::entropy`), `crypto.subtle.digest` (SHA-1/256/384/512): native classes in
//!   `lumen_host::webcrypto`, published lazily
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
//! - [x] `WebSocket` and `EventSource`: native classes (`websocket_class.rs`, `eventsource_class.rs`)
//!   over the transports in `websocket.rs` and `sse.rs`, published lazily
//! - [x] `URLPattern`: native class in `lumen_host::url_pattern` over the vendored `urlpattern` crate,
//!   whose component expressions run on `lumen_common::regex`; published lazily
//! - [ ] `crypto.subtle` beyond digest

use lumen_bind::NativeError;
#[cfg(not(target_arch = "wasm32"))]
use lumen_host::SpawnHandle;
use lumen_host::{Ctx, Extension, OpError, OpState, Value};

#[lumen_bind::module(name = "__http_policy")]
mod http_policy {
    use super::*;

    #[lumen_bind::op(name = "policyHandledByHost")]
    pub fn policy_handled_by_host() -> bool {
        cfg!(target_arch = "wasm32")
    }

    #[lumen_bind::op(name = "requestSync")]
    pub fn request_sync(
        ctx: &mut Ctx,
        method: String,
        url: String,
        headers: Value,
        body: Option<Value>,
        options: Value,
    ) -> lumen::embed::OpResult<Value> {
        use lumen::embed::OpError;

        let headers = read_header_pairs(ctx, &headers).map_err(OpError::thrown)?;
        let body = body
            .map(|value| {
                ctx.typed_array_bytes(&value)
                    .ok_or_else(|| OpError::new("TypeError", "HTTP body must be bytes"))
            })
            .transpose()?;
        let option_string = |ctx: &mut Ctx, name: &str, default: &str| -> Result<String, OpError> {
            let value = ctx.member_get(&options, name).map_err(OpError::thrown)?;
            if matches!(value, Value::Undefined) {
                Ok(default.to_owned())
            } else {
                Ok(ctx
                    .coerce_string(&value)
                    .map_err(OpError::thrown)?
                    .to_string())
            }
        };
        let mode = option_string(ctx, "mode", "cors")?;
        let credentials = option_string(ctx, "credentials", "same-origin")?;
        let redirect = option_string(ctx, "redirect", "follow")?;
        let force_preflight = matches!(
            ctx.member_get(&options, "forcePreflight")
                .map_err(OpError::thrown)?,
            Value::Bool(true)
        );
        let timeout_ms = match ctx
            .member_get(&options, "timeout")
            .map_err(OpError::thrown)?
        {
            Value::Undefined | Value::Null => 0,
            value => {
                let timeout = ctx.coerce_number(&value).map_err(OpError::thrown)?;
                if !timeout.is_finite() || timeout < 0.0 || timeout > u32::MAX as f64 {
                    return Err(OpError::new("TypeError", "invalid synchronous XHR timeout"));
                }
                timeout as u32
            }
        };
        if !matches!(mode.as_str(), "cors" | "no-cors" | "same-origin") {
            return Err(OpError::new("TypeError", "invalid fetch mode"));
        }
        if !matches!(credentials.as_str(), "omit" | "same-origin" | "include") {
            return Err(OpError::new("TypeError", "invalid credentials mode"));
        }
        if !matches!(redirect.as_str(), "follow" | "manual" | "error") {
            return Err(OpError::new("TypeError", "invalid transport redirect mode"));
        }
        let origin = match ctx
            .member_get(&options, "origin")
            .map_err(OpError::thrown)?
        {
            Value::Undefined | Value::Null => None,
            value => Some(
                ctx.coerce_string(&value)
                    .map_err(OpError::thrown)?
                    .to_string(),
            ),
        };
        let request = lumen_common::http_body::SyncHttpRequest {
            method,
            url: url.clone(),
            headers,
            body,
            mode,
            credentials,
            redirect: redirect.clone(),
            force_preflight,
            timeout_ms,
            origin,
        };
        #[cfg(target_arch = "wasm32")]
        let response = {
            let encoded = request
                .encode()
                .map_err(|error| OpError::new("TypeError", error.0))?;
            let encoded =
                lumen_host::browser::sync_call("http.request", &encoded).map_err(|error| {
                    if let Some(message) = error.strip_prefix("TimeoutError: ") {
                        OpError::new("TimeoutError", message.to_owned())
                    } else if let Some(message) = error.strip_prefix("NotSupportedError: ") {
                        OpError::new("NotSupportedError", message.to_owned())
                    } else {
                        OpError::new("NetworkError", error)
                    }
                })?;
            lumen_common::http_body::SyncHttpResponse::decode(&encoded)
                .map_err(|error| OpError::new("NetworkError", error.0))?
        };
        #[cfg(not(target_arch = "wasm32"))]
        let response = {
            sync_http_desktop(&request, &FetchConfig::default()).map_err(|error| match error {
                http::SyncRequestError::Timeout => {
                    OpError::new("TimeoutError", "synchronous XMLHttpRequest timed out")
                }
                http::SyncRequestError::Transport(error) => OpError::new("NetworkError", error),
            })?
        };
        let output = Value::Obj(ctx.new_object());
        let set = |ctx: &mut Ctx, name: &str, value: Value| {
            ctx.member_set(&output, name, value)
                .map_err(OpError::thrown)
        };
        set(ctx, "status", Value::Num(response.status as f64))?;
        set(ctx, "statusText", Value::from_string(response.status_text))?;
        set(ctx, "url", Value::from_string(response.url.clone()))?;
        set(ctx, "redirected", Value::Bool(response.url != url))?;
        set(ctx, "type", Value::from_string("basic".into()))?;
        let pairs = response
            .headers
            .into_iter()
            .map(|(key, value)| {
                ctx.make_array(vec![Value::from_string(key), Value::from_string(value)])
            })
            .collect();
        let pairs = ctx.make_array(pairs);
        set(ctx, "headers", pairs)?;
        let bytes = ctx
            .make_uint8array(&response.body)
            .map_err(OpError::thrown)?;
        set(ctx, "body", bytes)?;
        Ok(output)
    }
}

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
mod eventsource_class;
mod net_class;
mod websocket_class;

#[cfg(not(target_arch = "wasm32"))]
fn sync_http_desktop(
    request: &lumen_common::http_body::SyncHttpRequest,
    config: &FetchConfig,
) -> Result<lumen_common::http_body::SyncHttpResponse, http::SyncRequestError> {
    use lumen_common::cors::{Credentials, FetchPolicy, Mode, Redirect, ResponseType};
    use std::time::{Duration, Instant};

    let Some(origin) = request.origin.as_deref() else {
        let mut config = config.clone();
        config.manual_redirect = request.redirect != "follow";
        let response = http::request_sync_with_timeout(
            &request.method,
            &request.url,
            &request.headers,
            request.body.as_deref(),
            &config,
            request.timeout_ms,
        )?;
        if request.redirect == "error"
            && matches!(response.status, 301 | 302 | 303 | 307 | 308)
            && response
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("location"))
        {
            return Err(http::SyncRequestError::Transport(
                "redirect mode is error".into(),
            ));
        }
        return Ok(lumen_common::http_body::SyncHttpResponse {
            status: response.status,
            status_text: response.status_text,
            url: response.url,
            headers: response.headers,
            body: response.body,
        });
    };

    let mode = match request.mode.as_str() {
        "cors" => Mode::Cors,
        "no-cors" => Mode::NoCors,
        "same-origin" => Mode::SameOrigin,
        _ => return Err(policy_transport("invalid fetch mode")),
    };
    let credentials = match request.credentials.as_str() {
        "omit" => Credentials::Omit,
        "same-origin" => Credentials::SameOrigin,
        "include" => Credentials::Include,
        _ => return Err(policy_transport("invalid credentials mode")),
    };
    let redirect = match request.redirect.as_str() {
        "follow" => Redirect::Follow,
        "error" => Redirect::Error,
        "manual" => Redirect::Manual,
        _ => return Err(policy_transport("invalid redirect mode")),
    };
    let parsed = lumen_common::url::parse_url(&request.url, None)
        .ok_or_else(|| policy_transport("invalid request URL"))?;
    let mut policy = FetchPolicy::new(
        origin,
        &request.method,
        &request.url,
        &parsed.origin(),
        request.headers.clone(),
        request.body.clone(),
        mode,
        credentials,
        redirect,
    )
    .map_err(|error| policy_transport(&format!("request policy rejected request: {error:?}")))?;
    policy.set_force_preflight(request.force_preflight);

    let started = Instant::now();
    let remaining = || -> Result<u32, http::SyncRequestError> {
        if request.timeout_ms == 0 {
            return Ok(0);
        }
        let budget = Duration::from_millis(request.timeout_ms as u64);
        let elapsed = started.elapsed();
        if elapsed >= budget {
            return Err(http::SyncRequestError::Timeout);
        }
        Ok((budget - elapsed).as_millis().clamp(1, u32::MAX as u128) as u32)
    };
    let mut config = config.clone();
    config.manual_redirect = true;

    loop {
        if let Some(preflight) = policy.preflight_request() {
            let response = http::request_sync_with_timeout(
                &preflight.method,
                &preflight.url,
                &preflight.headers,
                preflight.body.as_deref(),
                &config,
                remaining()?,
            )?;
            policy
                .validate_preflight(response.status, &response.headers)
                .map_err(|error| policy_transport(&format!("CORS preflight failed: {error:?}")))?;
        }

        let head = policy.actual_request();
        let response = http::request_sync_with_timeout(
            &head.method,
            &head.url,
            &head.headers,
            head.body.as_deref(),
            &config,
            remaining()?,
        )?;
        let location = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .map(|(_, value)| value.as_str());
        let next = location
            .and_then(|location| {
                let base = lumen_common::url::parse_url(policy.current_url(), None)?;
                lumen_common::url::parse_url(location, Some(&base))
            })
            .map(|url| (url.href(), url.origin()));
        let next_ref = next
            .as_ref()
            .map(|(url, origin)| (url.as_str(), origin.as_str()));
        let follow = policy
            .response_head(response.status, &response.headers, location, next_ref)
            .map_err(|error| {
                policy_transport(&format!("response policy rejected response: {error:?}"))
            })?;
        if follow {
            continue;
        }
        let filtered = policy.filter_response(&response.headers);
        let opaque_redirect = location.is_some()
            && matches!(response.status, 301 | 302 | 303 | 307 | 308)
            && redirect == Redirect::Manual;
        let opaque = filtered.kind == ResponseType::Opaque || opaque_redirect;
        return Ok(lumen_common::http_body::SyncHttpResponse {
            status: if opaque { 0 } else { response.status },
            status_text: if opaque {
                String::new()
            } else {
                response.status_text
            },
            url: if opaque { String::new() } else { response.url },
            headers: if opaque { Vec::new() } else { filtered.headers },
            body: if opaque { Vec::new() } else { response.body },
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn policy_transport(message: &str) -> http::SyncRequestError {
    http::SyncRequestError::Transport(message.to_owned())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod sync_http_desktop_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn synchronous_desktop_cors_forces_preflight_and_filters_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, status) in [(0, "204 No Content"), (1, "200 OK")] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("content-length:") {
                        content_length = line.split_once(':').unwrap().1.trim().parse().unwrap();
                    }
                    let done = line == "\r\n";
                    headers.push_str(&line);
                    if done {
                        break;
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).unwrap();
                requests.push(headers);
                let response = if index == 0 {
                    format!(
                        "HTTP/1.1 {status}\r\nAccess-Control-Allow-Origin: https://page.test\r\nAccess-Control-Allow-Credentials: true\r\nAccess-Control-Allow-Methods: POST\r\nAccess-Control-Allow-Headers: content-type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: https://page.test\r\nAccess-Control-Allow-Credentials: true\r\nAccess-Control-Expose-Headers: x-visible\r\nX-Visible: yes\r\nX-Hidden: no\r\nSet-Cookie: secret=1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_owned()
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });

        let mut config = FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("api.test", 80, address).unwrap();
        let request = lumen_common::http_body::SyncHttpRequest {
            method: "POST".into(),
            url: "http://api.test/resource".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: Some(b"{}".to_vec()),
            mode: "cors".into(),
            credentials: "include".into(),
            redirect: "follow".into(),
            force_preflight: true,
            timeout_ms: 3000,
            origin: Some("https://page.test".into()),
        };
        let response = sync_http_desktop(&request, &config).unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("OPTIONS /resource HTTP/1.1\r\n"));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("access-control-request-method: post"));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("access-control-request-headers: content-type"));
        assert!(requests[1].starts_with("POST /resource HTTP/1.1\r\n"));
        assert!(requests[1]
            .to_ascii_lowercase()
            .contains("origin: https://page.test"));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "x-visible" && value == "yes"));
        assert!(!response
            .headers
            .iter()
            .any(|(name, _)| name == "x-hidden" || name == "set-cookie"));
    }
}

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

/// Send a configured HTTP(S) request while requiring every redirect to stay
/// on the initial origin. Retains status, headers, body and final URL for
/// callers which install document responses after their own lifecycle checks.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_resource_same_origin_with_config(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &FetchConfig,
) -> Result<ResourceResponse, String> {
    http::request_with_config_same_origin(method, url, headers, body, config)
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
            lumen_host::namespace::<http_policy::Module>,
            url_namespace,
            lumen_host::lazy_globals::<lumen_host::encoding::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::url::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::url_pattern::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::events::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::events::internals::Module>,
            lumen_host::lazy_globals::<lumen_host::webcrypto::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::blob::internals::Module>,
            lumen_host::lazy_globals::<lumen_host::messaging::event_bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::messaging::channel_bindings::Module>,
            lumen_host::namespace::<lumen_host::messaging::shared::Module>,
            lumen_host::messaging::install_port_clone,
            lumen_host::lazy_globals::<lumen_host::structured_clone::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::structured_clone::internals::Module>,
            lumen_host::performance::install_globals,
            lumen_host::namespace::<http_ops::Module>,
            install_transport,
            lumen_host::lazy_globals::<lumen_host::net::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::net::fetch_bindings::Module>,
            install_lumen,
            lumen_host::lazy_globals::<websocket_class::bindings::Module>,
            lumen_host::lazy_globals::<eventsource_class::bindings::Module>,
            wasm_ops::install,
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
        lazy_globals: &[],
    }
}

/// One IIFE (preamble captures and deletes the raw `__*` namespaces, the rest defines the
/// standard classes over them), assembled by `build.rs` from `src/js/*.js` — the single source
/// of truth — and precompiled there to an ahead-of-time blob (AST, bytecode, compressed function
/// text), loaded at boot (see `lumen_host::install`).
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web_glue.aot"));
#[cfg(feature = "compiler")]
const JS_GLUE_SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/web_glue.js"));

/// Register the `__http` and `__http_policy` operations as the realm's native request transport.
fn install_transport(ctx: &mut Ctx) -> Result<(), Value> {
    let global = ctx.global_object();
    let http = ctx.member_get(&global, "__http")?;
    let policy = ctx.member_get(&global, "__http_policy")?;
    lumen_host::net::Transport::install(ctx, http, policy.clone(), policy);
    Ok(())
}

/// Publish `Lumen.serve`, `Lumen.upgradeWebSocket` and `Lumen.version` on the realm's `Lumen`
/// object, creating it when no earlier provider did.
fn install_lumen(ctx: &mut Ctx) -> Result<(), Value> {
    let global = ctx.global_object();
    let mut lumen = ctx.member_get(&global, "Lumen")?;
    if !matches!(lumen, Value::Obj(_)) {
        lumen = Value::Obj(ctx.new_object());
        ctx.member_set(&global, "Lumen", lumen.clone())?;
    }
    ctx.install_module::<server::Module>(&lumen)
}

fn url_namespace(ctx: &mut Ctx) -> Result<(), Value> {
    let ns = ctx.namespace_object("__url");
    ctx.install_module::<lumen_host::url::natives::Module>(&ns)
}

// ---- crypto ----

/// `n` cryptographically-random bytes (the WebSocket handshake key needs these, same source as
/// `crypto.getRandomValues`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn web_random_bytes(n: usize) -> Result<Vec<u8>, OpError> {
    lumen_host::webcrypto::random_bytes(n)
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
        fetch_options: Option<Value>,
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
        let upload_progress = std::sync::Arc::new(lumen_common::http_body::UploadProgress::new(
            body.as_ref().map(|bytes| bytes.len() as u64),
        ));
        let report_upload_progress = match fetch_options.filter(|value| value.as_obj().is_some()) {
            Some(options) => matches!(
                ctx.get_member(&options, "uploadProgress"),
                Ok(Value::Bool(true))
            ),
            None => false,
        };
        let worker_upload_progress = report_upload_progress.then(|| upload_progress.clone());
        let spawn = ctx
            .op_state()
            .get::<SpawnHandle>()
            .expect("runtime installs the spawn handle")
            .clone();
        spawn.spawn_blocking(id, move || {
            Box::new(http::open_request_cancellable_with_progress(
                &method,
                &target,
                &headers,
                body.as_deref(),
                &worker_cancellation,
                &fetch_config,
                worker_upload_progress,
            ))
        });
        Ok(ctx.new_instance(request_control::RequestControl {
            id,
            cancellation,
            upload_progress,
        }))
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
