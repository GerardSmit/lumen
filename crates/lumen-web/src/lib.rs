//! lumen-web — the WinterTC "Minimum Common Web Platform API", incrementally.
//!
//! Native `lumen_bind` Rust APIs, with a shared facade for Performance entry prototypes
//! and WebIDL conversion. Conformance checklist
//! against the WinterTC minimum common API:
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
//! - [x] `performance.now()` (+`timeOrigin`), user timing and `PerformanceObserver`:
//!   realm-owned traced Rust timeline and observer queues, shared with node:perf_hooks.
//!   `navigator.userAgent`: native `Navigator` in
//!   `lumen_host::navigator`, published lazily
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
//!   streams), which the runtime installs alongside this crate; this crate only consumes them.
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
#[cfg(not(target_arch = "wasm32"))]
use lumen_host::OpError;
use lumen_host::{Ctx, Extension, OpState, Value};

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
mod cookies;
#[cfg(not(target_arch = "wasm32"))]
pub use cookies::BrowserCookies;
#[cfg(not(target_arch = "wasm32"))]
mod http_body;
#[cfg(target_arch = "wasm32")]
#[path = "browser_body.rs"]
mod http_body;
mod request_control;
#[cfg(not(target_arch = "wasm32"))]
mod nbio;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod sse;
#[cfg(not(target_arch = "wasm32"))]
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
    sync_http_desktop_with_script(request, config, None)
}

/// Internal script fetch metadata is consumed before CORS response filtering.
/// It never exposes non-safelisted headers to the public Fetch API.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct ScriptFetchMetadata {
    pub destination: lumen_common::csp::Destination,
    pub self_url: String,
    pub nonce: String,
    pub integrity: String,
    pub parser_inserted: bool,
    pub referrer: lumen_common::referrer::Referrer,
    pub policies: std::sync::Arc<lumen_common::csp::PolicySet>,
    pub violations: Vec<lumen_common::csp::Violation>,
}

#[cfg(not(target_arch = "wasm32"))]
fn sync_http_desktop_with_script(
    request: &lumen_common::http_body::SyncHttpRequest,
    config: &FetchConfig,
    script: Option<&mut ScriptFetchMetadata>,
) -> Result<lumen_common::http_body::SyncHttpResponse, http::SyncRequestError> {
    sync_http_desktop_with_metadata(request,config,script,None,None,None)
}

#[cfg(not(target_arch = "wasm32"))]
fn sync_http_desktop_with_metadata(
    request: &lumen_common::http_body::SyncHttpRequest,
    config: &FetchConfig,
    mut script: Option<&mut ScriptFetchMetadata>,
    mut stylesheet_policy: Option<&mut StylesheetResponsePolicy>,
    cancellation:Option<&lumen_os::net::TcpCancellation>,
    mut response_redirects:Option<&mut u32>,
) -> Result<lumen_common::http_body::SyncHttpResponse, http::SyncRequestError> {
    use lumen_common::cors::{Credentials, FetchPolicy, Mode, Redirect, ResponseType};
    use std::time::{Duration, Instant};

    let Some(origin) = request.origin.as_deref() else {
        let mut config = config.clone();
        config.manual_redirect = request.redirect != "follow";
        let mut response = http::request_sync_with_timeout(
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
        response.body=lumen_common::http_body::decode_content_codings(&response.headers,response.body,
            usize::try_from(config.response_body_limit()).unwrap_or(usize::MAX))
            .map_err(|error|policy_transport(&format!("HTTP content decoding: {error}")))?;
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

    let mut redirect_count = 0;
    loop {
        if let Some(script) = script.as_deref_mut() {
            let decision = script.policies.check_resource_redirect(&request.url, policy.current_url(),
                &script.self_url, script.destination, &script.nonce, &script.integrity, script.parser_inserted, redirect_count)
                .map_err(|error| policy_transport(&format!("invalid script policy request: {error:?}")))?;
            script.violations.extend(decision.violations);
            if decision.blocked { return Err(policy_transport("script request blocked by Content Security Policy")); }
        }
        if let Some(preflight) = policy.preflight_request() {
            config.cookies_disabled = true;
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

        let mut head = policy.actual_request();
        if let Some(script) = script.as_deref() {
            let destination = lumen_common::url::parse(&head.url, None)
                .map_err(|_| policy_transport("invalid script request URL"))?;
            if let Some(referrer) = script.referrer.for_url(&destination) {
                // Referer is a user-agent header, not an author CORS header.
                head.headers.push(("Referer".into(), referrer));
            }
        }
        config.cookies_disabled = !policy.credentials_allowed();
        let mut response=if let Some(cancellation)=cancellation {
            http::request_sync_with_timeout_cancellable(&head.method,&head.url,&head.headers,head.body.as_deref(),&config,remaining()?,cancellation)?
        }else {
            http::request_sync_with_timeout(&head.method,&head.url,&head.headers,head.body.as_deref(),&config,remaining()?)?
        };
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
        if let Some(stylesheet)=stylesheet_policy.as_deref_mut() {
            stylesheet.timing_allowed &= policy.timing_allow_response(&response.headers);
        }
        let follow = policy
            .response_head(response.status, &response.headers, location, next_ref)
            .map_err(|error| {
                policy_transport(&format!("response policy rejected response: {error:?}"))
            })?;
        if let Some(script) = script.as_deref_mut() {
            script.referrer.apply_redirect_policy(&response.headers);
        }
        if follow {
            redirect_count += 1;
            continue;
        }
        if let Some(count)=response_redirects.as_deref_mut(){*count=redirect_count;}
        if let Some(script) = script.as_deref_mut() {
            let decision = script.policies.check_resource_response(&request.url, &response.url,
                &script.self_url, script.destination, &script.nonce, &script.integrity, script.parser_inserted, redirect_count)
                .map_err(|error| policy_transport(&format!("invalid script response policy: {error:?}")))?;
            script.violations.extend(decision.violations);
            if decision.blocked { return Err(policy_transport("script response blocked by Content Security Policy")); }
        }
        let filtered = policy.filter_response(&response.headers);
        let opaque_redirect = location.is_some()
            && matches!(response.status, 301 | 302 | 303 | 307 | 308)
            && redirect == Redirect::Manual;
        let opaque = filtered.kind == ResponseType::Opaque || opaque_redirect;
        let encoded_body_size=response.body.len() as u64;
        response.body=lumen_common::http_body::decode_content_codings(&response.headers,response.body,
            usize::try_from(config.response_body_limit()).unwrap_or(usize::MAX))
            .map_err(|error|policy_transport(&format!("HTTP content decoding: {error}")))?;
        if let Some(stylesheet)=stylesheet_policy.as_deref_mut() {
            stylesheet.encoded_body_size=encoded_body_size;
            stylesheet.decoded_body_size=response.body.len() as u64;
        }
        if let Some(script)=script.as_deref() {
            if !script.integrity.is_empty()
 && (opaque || !lumen_common::integrity::matches(&response.body,&script.integrity)) {
                return Err(policy_transport("script response failed Subresource Integrity"));
            }
        }
        // HTML stylesheet processing consumes the internal response even for
        // no-CORS resources. Public Fetch still receives its filtered response.
        if let Some(stylesheet)=stylesheet_policy.as_deref_mut() {
            stylesheet.origin_clean=!opaque;
            return Ok(lumen_common::http_body::SyncHttpResponse {
                status:response.status,status_text:response.status_text,url:response.url,
                headers:response.headers,body:response.body,
            });
        }
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
    fn specification_image_response_retains_real_redirect_metadata_and_internal_body() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move||{
            for index in 0..2 {
                let(mut stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader=BufReader::new(stream.try_clone().unwrap());
                loop{let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n"{break}}
                let response=if index==0{b"HTTP/1.1 302 Found\r\nLocation: http://final.image.test/pixel\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()}
                    else{b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".as_slice()};
                stream.write_all(response).unwrap();
            }
        });
        let mut config=FetchConfig::default();config.set_require_routes(true);config.set_route("initial.image.test",80,address).unwrap();config.set_route("final.image.test",80,address).unwrap();
        let(response,redirects)=load_image_resource_with_config("http://initial.image.test/request","http://page.test",&config,1024,3000).unwrap();
        assert_eq!(redirects,1);assert_eq!(response.url,"http://final.image.test/pixel");assert_eq!(response.body,b"abc");server.join().unwrap();
    }

    #[test]
    fn specification_script_integrity_checks_final_cors_response_bytes() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream,_)=listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader=BufReader::new(stream.try_clone().unwrap());
                loop {let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n" {break}}
                stream.write_all(b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: http://page.api.test\r\nContent-Type: text/javascript\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").unwrap();
            }
        });
        let mut config=FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("integrity.api.test",80,address).unwrap();
        let mut metadata=ScriptFetchMetadata {
            destination:lumen_common::csp::Destination::Script,self_url:"http://page.api.test/".into(),nonce:String::new(),
            integrity:"sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=".into(),parser_inserted:false,
            referrer:lumen_common::referrer::Referrer {source:"http://page.api.test/".into(),policy:lumen_common::referrer::ReferrerPolicy::Origin},
            policies:std::sync::Arc::new(lumen_common::csp::PolicySet::default()),violations:Vec::new(),
        };
        let response=load_script_resource_with_config("http://integrity.api.test/module.js","http://page.api.test","omit",&config,1024,3000,&mut metadata).unwrap();
        assert_eq!(response.body,b"abc");
        metadata.integrity.push_str(" sha512-mismatch");
        assert!(load_script_resource_with_config("http://integrity.api.test/module.js","http://page.api.test","omit",&config,1024,3000,&mut metadata).is_err(),"integrity mismatch is a network failure before module decoding");
        server.join().unwrap();
    }

    #[test]
    fn specification_script_fetch_redirects_share_cors_credentials_referrer_and_response_policy() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let second = TcpListener::bind("127.0.0.1:0").unwrap();
        let first_address = first.local_addr().unwrap();
        let second_address = second.local_addr().unwrap();
        let serve = |listener: TcpListener, response: &'static [u8]| std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let done = line == "\r\n";
                headers.push_str(&line);
                if done { break; }
            }
            stream.write_all(response).unwrap();
            headers.to_ascii_lowercase()
        });
        let first_server = serve(first, b"HTTP/1.1 302 Found\r\nLocation: http://second.api.test/module.js\r\nAccess-Control-Allow-Origin: http://page.api.test\r\nAccess-Control-Allow-Credentials: true\r\nReferrer-Policy: no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let second_server = serve(second, b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: null\r\nAccess-Control-Allow-Credentials: true\r\nContent-Type: text/javascript\r\nReferrer-Policy: origin\r\nX-Private: hidden\r\nContent-Length: 17\r\nConnection: close\r\n\r\nexport default 1;");
        let mut config = FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("first.api.test",80,first_address).unwrap();
        config.set_route("second.api.test",80,second_address).unwrap();
        let url = lumen_common::url::parse_url("http://second.api.test/module.js",None).unwrap();
        let cookie_context = lumen_common::cookies::Context::document(&url);
        let cookies = BrowserCookies::default();
        assert!(cookies.write(&url,"session=present; Path=/",&cookie_context,false));
        config.set_browser_cookies(cookies,cookie_context);
        let mut metadata = ScriptFetchMetadata {
            destination:lumen_common::csp::Destination::Script,
            self_url:"http://page.api.test/document".into(), nonce:"captured-nonce".into(), integrity:String::new(), parser_inserted:false,
            referrer:lumen_common::referrer::Referrer {source:"http://page.api.test/source.js?private=1".into(),policy:lumen_common::referrer::ReferrerPolicy::Origin},
            policies:std::sync::Arc::new(lumen_common::csp::PolicySet::default()), violations:Vec::new(),
        };
        let response = load_script_resource_with_config("http://first.api.test/start.js","http://page.api.test","include",&config,1024,3000,&mut metadata).unwrap();
        let first_request = first_server.join().unwrap();
        let second_request = second_server.join().unwrap();
        assert!(first_request.starts_with("get "));
        assert!(first_request.contains("\r\norigin: http://page.api.test\r\n"));
        assert!(first_request.contains("\r\nreferer: http://page.api.test/\r\n"));
        assert!(!first_request.contains("nonce"));
        assert!(second_request.contains("\r\norigin: null\r\n"));
        assert!(second_request.contains("\r\ncookie: session=present\r\n"));
        assert!(!second_request.contains("\r\nreferer:"));
        assert_eq!(response.url,"http://second.api.test/module.js");
        assert_eq!(response.body,b"export default 1;");
        assert_eq!(metadata.referrer.policy,lumen_common::referrer::ReferrerPolicy::Origin);
        assert!(!response.headers.iter().any(|(name,_)|name.eq_ignore_ascii_case("referrer-policy") || name.eq_ignore_ascii_case("x-private")));
        assert!(metadata.violations.is_empty());
    }

    #[test]
    fn browser_cookies_obey_credentials_and_omit_preflight() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let done = line == "\r\n";
                    headers.push_str(&line);
                    if done { break; }
                }
                requests.push(headers);
                stream.write_all(b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: http://page.api.test\r\nAccess-Control-Allow-Credentials: true\r\nAccess-Control-Allow-Methods: GET\r\nSet-Cookie: response=stored; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            requests
        });
        let mut config = FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("api.test", 80, address).unwrap();
        let url = lumen_common::url::parse_url("http://api.test/resource", None).unwrap();
        let context = lumen_common::cookies::Context::document(&url);
        let cookies = BrowserCookies::default();
        assert!(cookies.write(&url, "session=present; Path=/", &context, false));
        config.set_browser_cookies(cookies.clone(), context.clone());
        for credentials in ["omit", "same-origin", "include"] {
            let request = lumen_common::http_body::SyncHttpRequest {
                method: "GET".into(), url: url.href(), headers: Vec::new(), body: None,
                mode: "cors".into(), credentials: credentials.into(), redirect: "follow".into(),
                force_preflight: credentials == "include", timeout_ms: 3000,
                origin: Some("http://page.api.test".into()),
            };
            sync_http_desktop(&request, &config).unwrap();
            if credentials != "include" { assert_eq!(cookies.read(&url, &context, false), "session=present"); }
        }
        let requests = server.join().unwrap();
        for request in &requests[..3] { assert!(!request.to_ascii_lowercase().contains("\r\ncookie:")); }
        assert!(requests[2].starts_with("OPTIONS "));
        assert!(requests[3].to_ascii_lowercase().contains("\r\ncookie: session=present\r\n"));
        assert_eq!(cookies.read(&url, &context, false), "session=present; response=stored");
    }

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
    cookies: Option<BrowserCookies>,
    cookie_context: Option<lumen_common::cookies::Context>,
    cookies_disabled: bool,
    routes: std::sync::Arc<std::collections::HashMap<(String, u16), FetchRoute>>,
    require_routes: bool,
    /// Internal single-hop policy for the Fetch origin adapter.
    manual_redirect: bool,
    /// A caller-specific cap, enforced by the streaming HTTP body reader.
    response_body_limit: Option<u64>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(crate) struct FetchRoute {
    pub(crate) address: std::net::SocketAddr,
    pub(crate) extra_ca_pem: Option<std::sync::Arc<[u8]>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FetchConfig {
    /// Attach the embedding browser's jar and trusted top-level browsing context.
    pub fn set_browser_cookies(&mut self, cookies: BrowserCookies, context: lumen_common::cookies::Context) {
        self.cookies = Some(cookies);
        self.cookie_context = Some(context);
    }
    pub fn browser_cookies(&self) -> Option<(BrowserCookies, lumen_common::cookies::Context)> {
        Some((self.cookies.clone()?, self.cookie_context.clone()?))
    }

    /// Restrict a resource request before allocating its response body. Clones retain the cap
    /// across redirects; ordinary Fetch keeps its existing global limit.
    pub fn set_response_body_limit(&mut self, limit: usize) {
        self.response_body_limit = Some((limit as u64).min(http::MAX_BODY));
    }

    pub(crate) fn response_body_limit(&self) -> u64 {
        self.response_body_limit.unwrap_or(http::MAX_BODY)
    }
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

/// Load a bounded CORS resource using the existing Fetch policy, including redirect checks,
/// fixture routing and verified TLS. Intended for host-managed font loads on an I/O thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_cors_resource_with_config(
    url: &str,
    origin: &str,
    config: &FetchConfig,
    body_limit: usize,
    timeout_ms: u32,
) -> Result<lumen_common::http_body::SyncHttpResponse, String> {
    let mut config = config.clone();
    config.set_response_body_limit(body_limit);
    let request = lumen_common::http_body::SyncHttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers: Vec::new(),
        body: None,
        mode: "cors".into(),
        credentials: "same-origin".into(),
        redirect: "follow".into(),
        force_preflight: false,
        timeout_ms,
        origin: Some(origin.into()),
    };
    sync_http_desktop(&request, &config).map_err(|error| format!("resource fetch: {error:?}"))
}

/// Module-script CORS fetch, sharing the normal Fetch redirect/credential path.
/// Metadata remains available on failure so report-only/enforced CSP violations
/// can be delivered in the captured client realm by the host.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_script_resource_with_config(
    url: &str, origin: &str, credentials: &str, config: &FetchConfig,
    body_limit: usize, timeout_ms: u32, metadata: &mut ScriptFetchMetadata,
) -> Result<lumen_common::http_body::SyncHttpResponse, String> {
    let mut config = config.clone();
    config.set_response_body_limit(body_limit);
    let request = lumen_common::http_body::SyncHttpRequest {
        method: "GET".into(), url: url.into(), headers: Vec::new(), body: None,
        mode: "cors".into(), credentials: credentials.into(), redirect: "follow".into(),
        force_preflight: false, timeout_ms, origin: Some(origin.into()),
    };
    sync_http_desktop_with_script(&request, &config, Some(metadata))
        .map_err(|error| format!("script resource fetch: {error:?}"))
}

/// Internal response policies remain independent: CORS cleanliness is not TAO.
#[derive(Clone, Copy)]
pub struct StylesheetResponsePolicy {
    pub origin_clean:bool,
    pub timing_allowed:bool,
    pub encoded_body_size:u64,
    pub decoded_body_size:u64,
}

/// Privileged HTML stylesheet consumer using the same potential-CORS,
/// redirects, credentials, CSP, referrer and integrity controller as Fetch.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_stylesheet_resource_with_config(
    url:&str,origin:&str,crossorigin:Option<bool>,config:&FetchConfig,
    body_limit:usize,timeout_ms:u32,metadata:&mut ScriptFetchMetadata,cancellation:Option<&lumen_os::net::TcpCancellation>,
)->Result<(lumen_common::http_body::SyncHttpResponse,StylesheetResponsePolicy),String> {
    let mut config=config.clone();config.set_response_body_limit(body_limit);
    let request=lumen_common::http_body::SyncHttpRequest {
        method:"GET".into(),url:url.into(),headers:Vec::new(),body:None,
        mode:if crossorigin.is_some(){"cors"}else{"no-cors"}.into(),
        credentials:if crossorigin==Some(false){"same-origin"}else{"include"}.into(),
        redirect:"follow".into(),force_preflight:false,timeout_ms,origin:Some(origin.into()),
    };
    let mut policy=StylesheetResponsePolicy {origin_clean:false,timing_allowed:true,encoded_body_size:0,decoded_body_size:0};
    let response=sync_http_desktop_with_metadata(&request,&config,Some(metadata),Some(&mut policy),cancellation,None)
        .map_err(|error|format!("stylesheet fetch: {error:?}"))?;
    Ok((response,policy))
}

/// Load an internal image response with actual redirect metadata. The shared
/// Fetch controller applies routing, credentials, redirects and body limits;
/// the resource consumer receives the internal body before public filtering.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_image_resource_with_config(url:&str,origin:&str,config:&FetchConfig,body_limit:usize,timeout_ms:u32)
    ->Result<(lumen_common::http_body::SyncHttpResponse,u32),String> {
    let mut config=config.clone();config.set_response_body_limit(body_limit);
    let request=lumen_common::http_body::SyncHttpRequest{method:"GET".into(),url:url.into(),headers:Vec::new(),body:None,
        mode:"no-cors".into(),credentials:"include".into(),redirect:"follow".into(),force_preflight:false,timeout_ms,origin:Some(origin.into())};
    let mut policy=StylesheetResponsePolicy{origin_clean:false,timing_allowed:true,encoded_body_size:0,decoded_body_size:0};
    let mut redirects=0;
    let response=sync_http_desktop_with_metadata(&request,&config,None,Some(&mut policy),None,Some(&mut redirects))
        .map_err(|error|format!("image fetch: {error:?}"))?;
    Ok((response,redirects))
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

#[cfg(not(target_arch = "wasm32"))]
struct NavigationFetchState {
    referrer: lumen_common::referrer::Referrer,
    request_referrer: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
pub struct NavigationResponse {
    pub response: ResourceResponse,
    pub request_referrer: Option<String>,
}

/// Fetch a navigation using the shared redirect loop, recomputing the
/// referrer after each response's Referrer-Policy has been processed.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_navigation_resource_with_config(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &FetchConfig,
    referrer: lumen_common::referrer::Referrer,
) -> Result<NavigationResponse, String> {
    http::request_navigation_with_config(method, url, headers, body, config, referrer)
}

/// HTML embedded-document navigation consumes the internal response through
/// the ordinary navigation redirect controller, with each hop checked against
/// the captured destination's CSP. No public Fetch response filtering applies.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_embedded_navigation_resource_with_config(
    url: &str, config: &FetchConfig, body_limit: usize,
    metadata: &mut ScriptFetchMetadata, cancellation: &lumen_os::net::TcpCancellation,
) -> Result<NavigationResponse, String> {
    let mut config = config.clone();
    config.set_response_body_limit(body_limit);
    let mut response = http::request_embedded_navigation_with_config(url, &config, metadata, cancellation)?;
    response.response.body = lumen_common::http_body::decode_content_codings(
        &response.response.headers, response.response.body, body_limit,
    ).map_err(|error| format!("embedded resource content decoding: {error}"))?;
    Ok(response)
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

/// Close every HTTP listener the realm still holds, and drop the connections in flight.
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
            lumen_host::lazy_globals::<lumen_host::encoding::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::url::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::url_pattern::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::navigator::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::events::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::events::internals::Module>,
            lumen_host::lazy_globals::<lumen_host::webcrypto::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::blob::internals::Module>,
            lumen_host::messaging::install,
            lumen_host::lazy_globals::<lumen_host::structured_clone::bindings::Module>,
            lumen_host::lazy_globals::<lumen_host::structured_clone::internals::Module>,
            lumen_host::performance::install_globals,
            lumen_host::namespace::<lumen_host::performance_timeline::bindings::Module>,
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
        js_init: Some(lumen_host::performance_timeline::SOURCE),
        js_init_snapshot: Some(include_bytes!(concat!(env!("OUT_DIR"), "/performance_timeline.aot"))),
        lazy_globals: &[],
    }
}

/// Register the `__http` and `__http_policy` operations as the realm's native request transport.
fn install_transport(ctx: &mut Ctx) -> Result<(), Value> {
    let global = ctx.global_object();
    let http = ctx.member_get(&global, "__http")?;
    let policy = ctx.member_get(&global, "__http_policy")?;
    lumen_host::net::Transport::install(ctx, http, policy.clone(), policy).map_err(|error|error.to_value(ctx))?;
    ctx.delete_member(&global, "__http")?;
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
            Some(options) => {
                fetch_config.cookies_disabled = !matches!(ctx.get_member(&options, "cookiesAllowed"), Ok(Value::Bool(true)));
                matches!(ctx.get_member(&options, "uploadProgress"), Ok(Value::Bool(true)))
            },
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
