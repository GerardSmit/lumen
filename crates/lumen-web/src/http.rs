//! A blocking HTTP/1.1 client on `std::net::TcpStream`, run on the threadpool by the fetch
//! op. `Connection: close` per request (no pooling), Content-Length and chunked bodies,
//! redirects followed up to 5 hops. HTTPS goes through lumen-tls (the dynamically loaded system
//! OpenSSL on Unix, rustls on Windows) with CA and hostname verification.

use std::io::{BufReader, Read, Write};
use std::time::Duration;

use crate::url;
pub(crate) use lumen_os::http_body::read_capped_line;
use lumen_os::http_body::BodyReader;

pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Where the response actually came from (after redirects).
    pub url: String,
}

impl HttpResponse {
    /// The normalized MIME essence, independent of any response parameters.
    pub fn content_type(&self) -> Option<String> {
        self.response_mime_type()
            .and_then(|value| lumen_common::mime::mime_essence(&value).map(str::to_ascii_lowercase))
    }

    /// Fetch's extracted response MIME, including effective charset parameters.
    pub fn response_mime_type(&self) -> Option<String> {
        lumen_common::mime::extract_mime_type(self.headers.iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str()))
    }
}

pub(crate) struct OpenHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: OpenHttpBody,
    pub url: String,
}

pub(crate) struct OpenHttpBody {
    reader: Option<BodyReader<BufReader<Box<dyn ReadWrite>>>>,
    pub cancellation: lumen_os::net::TcpCancellation,
}

impl OpenHttpBody {
    pub fn is_empty(&self) -> bool {
        self.reader.as_ref().is_none_or(|reader| !reader.has_body())
    }
    pub fn read_chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let result = if self.cancellation.is_cancelled() {
            Err(std::io::ErrorKind::Interrupted.into())
        } else {
            match self.reader.as_mut() {
                Some(reader) => reader.read_chunk(64 << 10),
                None => Ok(None),
            }
        };
        if !matches!(result, Ok(Some(_))) {
            self.reader = None;
            self.cancellation.detach();
        }
        result
    }
}

impl Drop for OpenHttpBody {
    fn drop(&mut self) {
        self.cancellation.detach();
    }
}

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Response-body cap so a hostile server can't balloon the worker (32 MiB).
pub(crate) const MAX_BODY: u64 = 32 << 20;
/// Cap on the status/request line plus headers (and on chunked trailers).
pub(crate) const MAX_HEADER_BYTES: usize = 64 << 10;
pub(crate) const INITIAL_BODY_CAPACITY: u64 = 64 << 10;

pub(crate) fn request(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse, String> {
    request_cancellable(
        method,
        target,
        headers,
        body,
        &lumen_os::net::TcpCancellation::default(),
    )
}

pub(crate) fn request_cancellable(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
) -> Result<HttpResponse, String> {
    let response = open_request_cancellable(method, target, headers, body, cancellation)?;
    collect_response(response)
}

pub(crate) fn request_with_config(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &crate::FetchConfig,
) -> Result<HttpResponse, String> {
    request_cancellable_with_config(
        method,
        target,
        headers,
        body,
        &lumen_os::net::TcpCancellation::default(),
        config,
    )
}

/// Fetch with the same route/trust configuration as `request_with_config`, while requiring the
/// initial URL and every redirect target to retain the initial HTTP origin.
pub(crate) fn request_with_config_same_origin(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &crate::FetchConfig,
) -> Result<HttpResponse, String> {
    let response = open_request_cancellable_with_config_same_origin(
        method,
        target,
        headers,
        body,
        &lumen_os::net::TcpCancellation::default(),
        config,
    )?;
    collect_response(response)
}

pub(crate) fn request_navigation_with_config(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &crate::FetchConfig,
    referrer: lumen_common::referrer::Referrer,
) -> Result<crate::NavigationResponse, String> {
    let mut state = crate::NavigationFetchState { referrer, request_referrer: None };
    let response = open_request_cancellable_with_redirect_origin(
        method, target, headers, body, &lumen_os::net::TcpCancellation::default(),
        config, None, None, Some(&mut state), None,
    )?;
    Ok(crate::NavigationResponse {
        response: collect_response(response)?, request_referrer: state.request_referrer,
    })
}

pub(crate) fn request_embedded_navigation_with_config(
    target: &str,
    config: &crate::FetchConfig,
    metadata: &mut crate::ScriptFetchMetadata,
    cancellation: &lumen_os::net::TcpCancellation,
) -> Result<crate::NavigationResponse, String> {
    let mut state = crate::NavigationFetchState {
        referrer: metadata.referrer.clone(), request_referrer: None,
    };
    let response = open_request_cancellable_with_redirect_origin(
        "GET", target, &[], None, cancellation, config, None, None,
        Some(&mut state), Some(metadata),
    )?;
    Ok(crate::NavigationResponse {
        response: collect_response(response)?, request_referrer: state.request_referrer,
    })
}

pub(crate) fn request_cancellable_with_config(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
) -> Result<HttpResponse, String> {
    let response =
        open_request_cancellable_with_config(method, target, headers, body, cancellation, config)?;
    collect_response(response)
}

#[derive(Debug)]
pub(crate) enum SyncRequestError {
    Timeout,
    Transport(String),
}

/// Synchronous XHR transport for hosts that own a blocking socket runtime. The
/// deadline cancels the live socket, interrupting connect/read/write/TLS work.
pub(crate) fn request_sync_with_timeout(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &crate::FetchConfig,
    timeout_ms: u32,
) -> Result<HttpResponse, SyncRequestError> {
    request_sync_with_timeout_on(
        lumen_os::sched::current(),
        method,
        target,
        headers,
        body,
        config,
        timeout_ms,
    )
}

/// The same bounded request with an externally owned socket cancellation.
/// Cancelling a stylesheet ticket interrupts the existing live transport.
pub(crate) fn request_sync_with_timeout_cancellable(
    method:&str,target:&str,headers:&[(String,String)],body:Option<&[u8]>,config:&crate::FetchConfig,
    timeout_ms:u32,cancellation:&lumen_os::net::TcpCancellation,
)->Result<HttpResponse,SyncRequestError> {
    request_sync_with_timeout_on_cancellable(lumen_os::sched::current(),method,target,headers,body,config,timeout_ms,cancellation)
}

/// The deadline is a timer of `scheduler`, armed for the length of the request and cancelled when
/// it finishes first; no thread waits for it.
fn request_sync_with_timeout_on(
    scheduler: &dyn lumen_os::sched::Scheduler,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    config: &crate::FetchConfig,
    timeout_ms: u32,
) -> Result<HttpResponse, SyncRequestError> {
    request_sync_with_timeout_on_cancellable(scheduler,method,target,headers,body,config,timeout_ms,
        &lumen_os::net::TcpCancellation::default())
}
fn request_sync_with_timeout_on_cancellable(
    scheduler:&dyn lumen_os::sched::Scheduler,method:&str,target:&str,headers:&[(String,String)],
    body:Option<&[u8]>,config:&crate::FetchConfig,timeout_ms:u32,cancellation:&lumen_os::net::TcpCancellation,
)->Result<HttpResponse,SyncRequestError> {
    let state =
 std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let timer = if timeout_ms == 0 {
        None
    } else {
        let timer_state = state.clone();
        let timer_cancellation = cancellation.clone();
        let timer = scheduler
            .after(
                Duration::from_millis(timeout_ms as u64),
                Box::new(move || {
                    if timer_state
                        .compare_exchange(
                            0,
                            2,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        timer_cancellation.cancel();
                    }
                }),
            )
            .map_err(|error| SyncRequestError::Transport(format!("XHR timeout timer: {error}")))?;
        Some(timer)
    };
    let result =
        request_cancellable_with_config(method, target, headers, body, &cancellation, config);
    let _ = state.compare_exchange(
        0,
        1,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    );
    drop(timer);
    if state.load(std::sync::atomic::Ordering::Acquire) == 2 {
        return Err(SyncRequestError::Timeout);
    }
    result.map_err(SyncRequestError::Transport)
}

fn collect_response(mut response: OpenHttpResponse) -> Result<HttpResponse, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .body
        .read_chunk()
        .map_err(|error| format!("fetch: body: {error}"))?
    {
        body.extend(chunk);
    }
    Ok(HttpResponse {
        status: response.status,
        status_text: response.status_text,
        headers: response.headers,
        body,
        url: response.url,
    })
}

pub(crate) fn open_request_cancellable(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
) -> Result<OpenHttpResponse, String> {
    open_request_cancellable_with_config(
        method,
        target,
        headers,
        body,
        cancellation,
        &crate::FetchConfig::default(),
    )
}

pub(crate) fn open_request_cancellable_with_config(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
) -> Result<OpenHttpResponse, String> {
    open_request_cancellable_with_progress(
        method,
        target,
        headers,
        body,
        cancellation,
        config,
        None,
    )
}

/// Request entry point used by the Fetch op to retain and update upload progress.
pub(crate) fn open_request_cancellable_with_progress(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
    upload: Option<std::sync::Arc<lumen_common::http_body::UploadProgress>>,
) -> Result<OpenHttpResponse, String> {
    open_request_cancellable_with_redirect_origin(
        method,
        target,
        headers,
        body,
        cancellation,
        config,
        None,
        upload,
        None,
        None,
    )
}

fn open_request_cancellable_with_config_same_origin(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
) -> Result<OpenHttpResponse, String> {
    let initial = url::parse(target, None)?;
    let origin = HttpOrigin::from_url(&initial)
        .ok_or_else(|| "fetch: same-origin requests require an HTTP(S) URL".to_string())?;
    open_request_cancellable_with_redirect_origin(
        method,
        target,
        headers,
        body,
        cancellation,
        config,
        Some(origin),
        None,
        None,
        None,
    )
}

fn open_request_cancellable_with_redirect_origin(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
    required_origin: Option<HttpOrigin>,
    upload: Option<std::sync::Arc<lumen_common::http_body::UploadProgress>>,
    mut navigation: Option<&mut crate::NavigationFetchState>,
    mut policy: Option<&mut crate::ScriptFetchMetadata>,
) -> Result<OpenHttpResponse, String> {
    let target_initial = target;
    let mut method = method.to_ascii_uppercase();
    let mut target = target.to_string();
    let mut headers = headers.to_vec();
    let mut body = body;
    for redirect_count in 0..=MAX_REDIRECTS {
        if cancellation.is_cancelled() {
            return Err("fetch: request aborted".into());
        }
        let u = url::parse(&target, None)?;
        if let Some(policy) = policy.as_deref_mut() {
            let decision = policy.policies.check_resource_redirect(target_initial, &target,
                &policy.self_url, policy.destination, &policy.nonce, &policy.integrity,
                policy.parser_inserted, redirect_count as u32)
                .map_err(|error| format!("fetch: invalid resource policy: {error:?}"))?;
            policy.violations.extend(decision.violations);
            if decision.blocked { return Err("fetch: resource blocked by Content Security Policy".into()); }
        }

        if let Some(navigation) = navigation.as_deref_mut() {
            navigation.request_referrer = navigation.referrer.for_url(&u);
            navigation.referrer.source = navigation.request_referrer.clone().unwrap_or_default();
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("referer"));
            if let Some(value) = &navigation.request_referrer {
                headers.push(("Referer".into(), value.clone()));
            }
        }
        if required_origin
            .as_ref()
            .is_some_and(|origin| !origin.matches(&u))
        {
            return Err(format!(
                "fetch: request URL '{}' violates the same-origin policy",
                u.href()
            ));
        }
        match u.scheme.as_str() {
            "http" | "https" => {}
            other => return Err(format!("fetch: unsupported scheme '{other}'")),
        }
        if let Some(jar) = &config.cookies {
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
            if !config.cookies_disabled {
                let mut context = config.cookie_context.clone().unwrap_or_else(|| lumen_common::cookies::Context::document(&u));
                context.safe_method = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
                let value = jar.read(&u, &context, true);
                if !value.is_empty() { headers.push(("Cookie".into(), value)); }
            }
        }
        let result = one_request(
            &method,
            &u,
            &headers,
            body.as_deref(),
            cancellation,
            config,
            upload.as_deref(),
        );
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                cancellation.detach();
                return Err(error);
            }
        };
        if let Some(policy) = policy.as_deref_mut() {
            let decision = policy.policies.check_resource_response(target_initial, &u.href(),
                &policy.self_url, policy.destination, &policy.nonce, &policy.integrity,
                policy.parser_inserted, redirect_count as u32)
                .map_err(|error| format!("fetch: invalid resource response policy: {error:?}"))?;
            policy.violations.extend(decision.violations);
            if decision.blocked { return Err("fetch: resource response blocked by Content Security Policy".into()); }
        }
        if !config.cookies_disabled {
            if let Some(jar) = &config.cookies {
                let mut context = config.cookie_context.clone().unwrap_or_else(|| lumen_common::cookies::Context::document(&u));
                context.safe_method = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
                for (name, value) in &response.headers {
                    if name.eq_ignore_ascii_case("set-cookie") { jar.write(&u, value, &context, true); }
                }
            }
        }
        if config.manual_redirect {
            return Ok(OpenHttpResponse {
                url: u.href(),
                ..response
            });
        }
        match response.status {
            301 | 302 | 303 | 307 | 308 => {
                if let Some(navigation) = navigation.as_deref_mut() {
                    navigation.referrer.apply_redirect_policy(&response.headers);
                }
                let Some(location) = header(&response.headers, "location") else {
                    return Ok(OpenHttpResponse {
                        url: u.href(),
                        ..response
                    });
                };
                let next = url::parse(&location, Some(&u.href()))?;
                if required_origin
                    .as_ref()
                    .is_some_and(|origin| !origin.matches(&next))
                {
                    return Err(format!(
                        "fetch: redirect from '{}' to '{}' violates the same-origin policy",
                        u.href(),
                        next.href()
                    ));
                }
                let default_port = |url: &url::Url| {
                    url.port
                        .unwrap_or(if url.scheme == "https" { 443 } else { 80 })
                };
                if next.scheme != u.scheme
                    || next.hostname() != u.hostname()
                    || default_port(&next) != default_port(&u)
                {
                    headers.retain(|(name, _)| {
                        !name.eq_ignore_ascii_case("authorization")
                            && !name.eq_ignore_ascii_case("proxy-authorization")
                            && !name.eq_ignore_ascii_case("cookie")
                    });
                }
                target = next.href();
                // 303 (and historically 301/302) switch to GET and drop the body.
                if (response.status == 303 && method != "HEAD")
                    || ((response.status == 301 || response.status == 302) && method == "POST")
                {
                    method = "GET".to_string();
                    body = None;
                    headers.retain(|(name, _)| {
                        !name.eq_ignore_ascii_case("content-type")
                            && !name.eq_ignore_ascii_case("content-encoding")
                            && !name.eq_ignore_ascii_case("content-language")
                            && !name.eq_ignore_ascii_case("content-location")
                    });
                }
            }
            _ => {
                return Ok(OpenHttpResponse {
                    url: u.href(),
                    ..response
                });
            }
        }
    }
    Err(format!("fetch '{target}': too many redirects"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HttpOrigin {
    scheme: String,
    hostname: String,
    port: u16,
}

impl HttpOrigin {
    fn from_url(url: &url::Url) -> Option<Self> {
        let default_port = match url.scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        Some(Self {
            scheme: url.scheme.clone(),
            hostname: url.hostname().to_owned(),
            port: url.port.unwrap_or(default_port),
        })
    }

    fn matches(&self, url: &url::Url) -> bool {
        Self::from_url(url).as_ref() == Some(self)
    }
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn one_request(
    method: &str,
    u: &url::Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    cancellation: &lumen_os::net::TcpCancellation,
    config: &crate::FetchConfig,
    upload: Option<&lumen_common::http_body::UploadProgress>,
) -> Result<OpenHttpResponse, String> {
    if method.is_empty() || !method.bytes().all(http_token_byte) {
        return Err("fetch: invalid HTTP method".into());
    }
    let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
    let route = config.route_for(u.hostname(), port);
    if route.is_none() && config.requires_routes() {
        return Err(format!(
            "fetch '{}': no fixture route configured for {}:{}",
            u.href(),
            u.hostname(),
            port
        ));
    }
    let stream = match route.as_ref() {
        Some(route) => {
            lumen_os::net::connect_socket_addr_cancellable(route.address, TIMEOUT, cancellation)
        }
        None => lumen_os::net::connect_cancellable(
            u.hostname().trim_matches(['[', ']']),
            port,
            TIMEOUT,
            cancellation,
        ),
    }
    .map_err(|e| format!("fetch '{}': connect: {e}", u.href()))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();
    cancellation
        .attach(&stream)
        .map_err(|error| format!("fetch '{}': cancellation setup: {error}", u.href()))?;

    let req = lumen_common::http_body::request_head(
        method,
        &u,
        headers,
        body.map(<[u8]>::len),
        concat!("lumen/", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|error| format!("fetch '{}': request head: {error}", u.href()))?;

    let mut stream: Box<dyn ReadWrite> = if u.scheme == "https" {
        let hostname = u.hostname().trim_matches(['[', ']']);
        let tls = match route
            .as_ref()
            .and_then(|route| route.extra_ca_pem.as_deref())
        {
            Some(extra_roots) => {
                lumen_tls::TlsStream::connect_with_extra_roots(stream, hostname, extra_roots)
            }
            None => lumen_tls::TlsStream::connect(stream, hostname),
        };
        Box::new(tls.map_err(|error| format!("fetch '{}': {error}", u.href()))?)
    } else {
        Box::new(stream)
    };
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("fetch '{}': write: {e}", u.href()))?;
    if let Some(body) = body {
        for chunk in body.chunks(16 << 10) {
            let mut written = 0;
            while written < chunk.len() {
                let count = stream
                    .write(&chunk[written..])
                    .map_err(|e| format!("fetch '{}': write body: {e}", u.href()))?;
                if count == 0 {
                    return Err(format!(
                        "fetch '{}': write body: zero-length write",
                        u.href()
                    ));
                }
                written += count;
                if let Some(upload) = upload {
                    upload.record_written(count);
                }
            }
        }
    }
    stream
        .flush()
        .map_err(|e| format!("fetch '{}': flush request: {e}", u.href()))?;
    if let Some(upload) = upload {
        upload.mark_complete();
    }

    let mut reader = BufReader::new(stream);
    let mut header_budget = MAX_HEADER_BYTES;
    let (status, status_text, headers_out) = loop {
        let mut bytes = Vec::new();
        loop {
            let line = read_capped_line(&mut reader, &mut header_budget)
                .map_err(|error| format!("fetch '{}': read response head: {error}", u.href()))?;
            if line.is_empty() {
                return Err(format!("fetch '{}': truncated response head", u.href()));
            }
            let done = line == "\r\n";
            bytes.extend_from_slice(line.as_bytes());
            if done {
                break;
            }
        }
        let head = lumen_common::http_body::response_head(&bytes)
            .map_err(|error| format!("fetch '{}': response head: {error}", u.href()))?;
        if head.status >= 200 {
            break (head.status, head.status_text, head.headers);
        }
        // Informational responses share the header budget and transport. The final response,
        // rather than an interim 100/103, is delivered to Fetch.
    };

    let framing = lumen_os::http_body::framing(method, status, &headers_out)
        .map_err(|error| format!("fetch '{}': framing: {error}", u.href()))?;
    let reader = BodyReader::new(reader, framing, config.response_body_limit())
        .map_err(|error| format!("fetch '{}': body: {error}", u.href()))?;
    Ok(OpenHttpResponse {
        status,
        status_text,
        headers: headers_out,
        body: OpenHttpBody {
            reader: Some(reader),
            cancellation: cancellation.clone(),
        },
        url: String::new(), // stamped by the redirect loop
    })
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

fn http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    fn serve_http_once(
        listener: TcpListener,
        response: &'static str,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                request.push_str(&line);
                if end {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            request
        })
    }

    #[test]
    fn endless_header_line_is_rejected_at_the_cap() {
        struct Endless(usize);
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                buf.fill(b'a');
                Ok(buf.len())
            }
        }
        let mut reader = BufReader::new(Endless(0));
        let mut budget = MAX_HEADER_BYTES;
        assert!(read_capped_line(&mut reader, &mut budget).is_err());
        assert!(reader.get_ref().0 <= MAX_HEADER_BYTES + 2 * 8192);
    }

    #[test]
    fn bounded_cors_resources_preserve_routes_and_reject_oversized_heads() {
        for (response, expected) in [
            ("HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: https://page.test\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok", "ok"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok", "cors"),
            // The server sends no body: rejection must happen from the framing limit rather
            // than first allocating or attempting to read the advertised payload.
            ("HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: https://page.test\r\nContent-Length: 33554432\r\nConnection: close\r\n\r\n", "size"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config = crate::FetchConfig::default();
            config.set_require_routes(true);
            config.set_route("font.test", 80, listener.local_addr().unwrap()).unwrap();
            let captured = config.clone();
            config.remove_route("font.test", 80).unwrap();
            let server = serve_http_once(listener, response);
            let result = crate::load_cors_resource_with_config(
                "http://font.test/face.ttf", "https://page.test", &captured, 2, 3000,
            );
            let request = server.join().unwrap();
            assert!(request.starts_with("GET /face.ttf HTTP/1.1\r\n"));
            assert!(request.to_ascii_lowercase().contains("origin: https://page.test"));
            match expected {
                "ok" => assert_eq!(result.unwrap().body, b"ok"),
                "cors" => assert!(result.err().unwrap().contains("response policy rejected")),
                _ => assert!(result.is_err()),
            }
            assert_eq!(captured.response_body_limit(), MAX_BODY);
        }
    }

    #[test]
    fn header_budget_is_shared_across_lines() {
        let raw = b"aaaa\r\nbbbb\r\n";
        let mut r = std::io::Cursor::new(&raw[..]);
        let mut budget = 10;
        assert_eq!(read_capped_line(&mut r, &mut budget).unwrap(), "aaaa\r\n");
        assert!(read_capped_line(&mut r, &mut budget).is_err());
    }

    #[test]
    fn routed_redirects_keep_logical_hosts_and_module_loader_uses_routes() {
        let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let second_address = second_listener.local_addr().unwrap();
        let first = serve_http_once(
            first_listener,
            "HTTP/1.1 302 Found\r\nLocation: http://target.test:8000/final.js\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let second = serve_http_once(
            second_listener,
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );

        let mut config = crate::FetchConfig::default();
        config
            .set_route("ORIGIN.TEST", 8000, first_address)
            .unwrap();
        config
            .set_route("target.test", 8000, second_address)
            .unwrap();
        let module =
            crate::load_module_resource_with_config("http://origin.test:8000/start.js", &config)
                .unwrap();

        let first_request = first.join().unwrap();
        let second_request = second.join().unwrap();
        assert!(first_request.starts_with("GET /start.js HTTP/1.1\r\n"));
        assert!(first_request.contains("\r\nHost: origin.test:8000\r\n"));
        assert!(second_request.starts_with("GET /final.js HTTP/1.1\r\n"));
        assert!(second_request.contains("\r\nHost: target.test:8000\r\n"));
        assert_eq!(module.url, "http://target.test:8000/final.js");
        assert_eq!(module.content_type.as_deref(), Some("text/javascript"));
        assert_eq!(module.bytes, b"ok");
    }

    #[test]
    fn resource_response_keeps_redirected_http_failure_and_module_policy() {
        let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let second_address = second_listener.local_addr().unwrap();
        let first = serve_http_once(
            first_listener,
            "HTTP/1.1 302 Found\r\nLocation: http://target.test:8000/missing.css?v=2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let second = serve_http_once(
            second_listener,
            "HTTP/1.1 404 Not Found\r\nContent-Type: TEXT/CSS; charset=utf-8\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
        );
        let mut config = crate::FetchConfig::default();
        config
            .set_route("origin.test", 8000, first_address)
            .unwrap();
        config
            .set_route("target.test", 8000, second_address)
            .unwrap();
        config.set_require_routes(true);
        let response =
            crate::load_resource_with_config("http://origin.test:8000/start.css", &config).unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(response.url, "http://target.test:8000/missing.css?v=2");
        assert_eq!(response.content_type().as_deref(), Some("text/css"));
        assert_eq!(response.body, b"missing");
        assert!(
            first
                .join()
                .unwrap()
                .contains("\r\nHost: origin.test:8000\r\n")
        );
        let request = second.join().unwrap();
        assert!(request.starts_with("GET /missing.css?v=2 HTTP/1.1\r\n"));
        assert!(request.contains("\r\nHost: target.test:8000\r\n"));
        match crate::module_resource_from_response(response) {
            Ok(_) => panic!("module loading must reject HTTP failure"),
            Err(error) => assert!(error.contains("HTTP 404")),
        }
        assert!(
            crate::load_resource_with_config("http://unconfigured.test:8000/no.css", &config)
                .err()
                .unwrap()
                .contains("route")
        );
    }

    #[test]
    fn specification_window_navigation_redirect_recomputes_real_referrer_and_preserves_response() {
        let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let second_address = second_listener.local_addr().unwrap();
        let first = serve_http_once(first_listener,
            "HTTP/1.1 303 See Other\r\nLocation: http://target.test:8000/final\r\nReferrer-Policy: unsafe-url, no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let second = serve_http_once(second_listener,
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let mut config = crate::FetchConfig::default();
        config.set_route("origin.test", 8000, first_address).unwrap();
        config.set_route("target.test", 8000, second_address).unwrap();
        let response = crate::request_navigation_resource_with_config(
            "POST", "http://origin.test:8000/start", &[("Content-Type".into(),"text/plain".into())], Some(b"data"), &config,
            lumen_common::referrer::Referrer { source: "http://user:password@origin.test:8000/source?q#fragment".into(),
                policy: lumen_common::referrer::ReferrerPolicy::UnsafeUrl },
        ).unwrap();
        let first_request = first.join().unwrap();
        let second_request = second.join().unwrap();
        assert!(first_request.starts_with("POST /start HTTP/1.1\r\n"));
        assert!(first_request.to_ascii_lowercase().contains("\r\nreferer: http://origin.test:8000/source?q\r\n"));
        assert!(!first_request.contains("password"));
        assert!(second_request.starts_with("GET /final HTTP/1.1\r\n"));
        assert!(!second_request.to_ascii_lowercase().contains("\r\nreferer:"));
        assert!(!second_request.to_ascii_lowercase().contains("\r\ncontent-type:"));
        assert_eq!(response.request_referrer, None);
        assert_eq!(response.response.status, 404);
        assert_eq!(response.response.body, b"ok");
        assert_eq!(response.response.url, "http://target.test:8000/final");
    }

    #[test]
    fn specification_window_navigation_redirect_cannot_restore_reduced_referrer_source() {
        use lumen_common::referrer::{Referrer, ReferrerPolicy};
        for policy in [ReferrerPolicy::Origin, ReferrerPolicy::NoReferrer] {
            let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let first_address = first_listener.local_addr().unwrap();
            let second_address = second_listener.local_addr().unwrap();
            let first = serve_http_once(first_listener,
                "HTTP/1.1 302 Found\r\nLocation: http://target.test:8000/final\r\nReferrer-Policy: unsafe-url\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let second = serve_http_once(second_listener,
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let mut config = crate::FetchConfig::default();
            config.set_route("origin.test", 8000, first_address).unwrap();
            config.set_route("target.test", 8000, second_address).unwrap();
            let response = crate::request_navigation_resource_with_config("GET",
                "http://origin.test:8000/start", &[], None, &config,
                Referrer { source: "http://origin.test:8000/private?secret#fragment".into(), policy }).unwrap();
            let initial = first.join().unwrap().to_ascii_lowercase();
            let redirected = second.join().unwrap().to_ascii_lowercase();
            assert!(!redirected.contains("private"));
            assert!(!redirected.contains("secret"));
            if policy == ReferrerPolicy::Origin {
                assert!(initial.contains("\r\nreferer: http://origin.test:8000/\r\n"));
                assert!(redirected.contains("\r\nreferer: http://origin.test:8000/\r\n"));
                assert_eq!(response.request_referrer.as_deref(), Some("http://origin.test:8000/"));
            } else {
                assert!(!initial.contains("\r\nreferer:"));
                assert!(!redirected.contains("\r\nreferer:"));
                assert_eq!(response.request_referrer, None);
            }
        }
    }

    #[test]
    fn manual_redirect_returns_unfiltered_head_without_following_target() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://outside.test/final\r\nAccess-Control-Allow-Origin: https://document.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("origin.test", 80, address).unwrap();
        config.manual_redirect = true;
        let response = open_request_cancellable_with_config(
            "GET",
            "http://origin.test/start",
            &[],
            None,
            &lumen_os::net::TcpCancellation::default(),
            &config,
        )
        .unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.url, "http://origin.test/start");
        assert_eq!(
            header(&response.headers, "location").as_deref(),
            Some("http://outside.test/final")
        );
        assert_eq!(
            header(&response.headers, "access-control-allow-origin").as_deref(),
            Some("https://document.test")
        );
        server.join().unwrap();
    }

    #[test]
    fn upload_progress_tracks_first_body_write_and_freezes_across_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut received = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("content-length:") {
                        content_length = line.split_once(':').unwrap().1.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).unwrap();
                received.push(body);
                if index == 0 {
                    stream.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                }
            }
            received
        });
        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("upload.test", 80, address).unwrap();
        let body = vec![0x5a; 200_000];
        let progress = std::sync::Arc::new(lumen_common::http_body::UploadProgress::new(Some(
            body.len() as u64,
        )));
        let response = open_request_cancellable_with_progress(
            "POST",
            "http://upload.test/start",
            &[],
            Some(&body),
            &lumen_os::net::TcpCancellation::default(),
            &config,
            Some(progress.clone()),
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert!(progress.complete());
        assert_eq!(progress.loaded(), body.len() as u64);
        assert_eq!(progress.total(), body.len() as u64);
        let uploads = server.join().unwrap();
        assert_eq!(uploads, vec![body.clone(), body]);
        assert_eq!(progress.loaded(), progress.total());
    }

    #[test]
    fn synchronous_request_codec_reaches_real_http_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
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
                request.push_str(&line);
                if done {
                    break;
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            stream.write_all(b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\naccepted").unwrap();
            (request, body)
        });
        let request = lumen_common::http_body::SyncHttpRequest {
            method: "POST".into(),
            url: "http://sync.test/submit".into(),
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            body: Some(vec![0, 1, 0xff, b'x']),
            mode: "cors".into(),
            credentials: "same-origin".into(),
            redirect: "follow".into(),
            force_preflight: true,
            timeout_ms: 0,
            origin: None,
        };
        let request =
            lumen_common::http_body::SyncHttpRequest::decode(&request.encode().unwrap()).unwrap();
        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("sync.test", 80, address).unwrap();
        let response = request_with_config(
            &request.method,
            &request.url,
            &request.headers,
            request.body.as_deref(),
            &config,
        )
        .unwrap();
        let response = lumen_common::http_body::SyncHttpResponse {
            status: response.status,
            status_text: response.status_text,
            url: response.url,
            headers: response.headers,
            body: response.body,
        };
        let response =
            lumen_common::http_body::SyncHttpResponse::decode(&response.encode().unwrap()).unwrap();
        let (wire, body) = server.join().unwrap();
        assert!(wire.starts_with("POST /submit HTTP/1.1\r\n"));
        assert_eq!(body, [0, 1, 0xff, b'x']);
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"accepted");
    }

    #[test]
    fn synchronous_request_timeout_cancels_the_live_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(150));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("timeout.test", 80, address).unwrap();
        let result =
            request_sync_with_timeout("GET", "http://timeout.test/slow", &[], None, &config, 20);
        assert!(matches!(result, Err(SyncRequestError::Timeout)));
        server.join().unwrap();
    }

    /// A scheduler that forwards to an OS scheduler and counts the timers it hands out.
    struct CountingScheduler {
        inner: lumen_os::sched::OsScheduler,
        armed: std::sync::atomic::AtomicUsize,
        cancelled: std::sync::atomic::AtomicUsize,
    }

    struct CountedTimer {
        timer: lumen_os::sched::Timer,
        scheduler: &'static CountingScheduler,
    }

    impl lumen_os::sched::TimerCancel for CountedTimer {
        fn cancel(&self) -> bool {
            self.scheduler
                .cancelled
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.timer.cancel()
        }
    }

    impl lumen_os::sched::Scheduler for CountingScheduler {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn available_parallelism(&self) -> std::num::NonZeroUsize {
            self.inner.available_parallelism()
        }
        fn spawn_thread(
            &self,
            spec: lumen_os::sched::ThreadSpec,
            main: lumen_os::sched::ThreadMain,
        ) -> Result<lumen_os::sched::ThreadHandle, lumen_os::sched::SchedError> {
            self.inner.spawn_thread(spec, main)
        }
        fn spawn_blocking(
            &self,
            job: lumen_os::sched::Job,
        ) -> Result<(), lumen_os::sched::Job> {
            self.inner.spawn_blocking(job)
        }
        fn parker(
            &self,
        ) -> Result<std::sync::Arc<dyn lumen_os::sched::Park>, lumen_os::sched::SchedError> {
            self.inner.parker()
        }
        fn after(
            &self,
            delay: Duration,
            fire: lumen_os::sched::Job,
        ) -> Result<lumen_os::sched::Timer, lumen_os::sched::SchedError> {
            self.armed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let timer = self.inner.after(delay, fire)?;
            let scheduler: &'static CountingScheduler = counting();
            Ok(lumen_os::sched::Timer::new(std::sync::Arc::new(CountedTimer {
                timer,
                scheduler,
            })))
        }
    }

    fn counting() -> &'static CountingScheduler {
        static SCHEDULER: CountingScheduler = CountingScheduler {
            inner: lumen_os::sched::OsScheduler::new(),
            armed: std::sync::atomic::AtomicUsize::new(0),
            cancelled: std::sync::atomic::AtomicUsize::new(0),
        };
        &SCHEDULER
    }

    #[test]
    fn synchronous_timeout_is_a_scheduler_timer_that_completion_cancels() {
        use std::sync::atomic::Ordering::SeqCst;
        let scheduler = counting();
        let route = |listener: &TcpListener, config: &mut crate::FetchConfig| {
            config.set_require_routes(true);
            config
                .set_route("timer.test", 80, listener.local_addr().unwrap())
                .unwrap();
        };
        let serve = |listener: TcpListener, delay: Duration| {
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                std::thread::sleep(delay);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            })
        };

        // The request outlives its deadline: the timer fires and cancels the socket.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = crate::FetchConfig::default();
        route(&listener, &mut config);
        let server = serve(listener, Duration::from_millis(300));
        let armed = scheduler.armed.load(SeqCst);
        let started = std::time::Instant::now();
        let result = request_sync_with_timeout_on(
            scheduler,
            "GET",
            "http://timer.test/slow",
            &[],
            None,
            &config,
            50,
        );
        assert!(matches!(result, Err(SyncRequestError::Timeout)));
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(scheduler.armed.load(SeqCst), armed + 1);
        server.join().unwrap();

        // The request finishes first: the timer is cancelled before it could fire.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = crate::FetchConfig::default();
        route(&listener, &mut config);
        let server = serve(listener, Duration::ZERO);
        let (armed, cancelled) = (scheduler.armed.load(SeqCst), scheduler.cancelled.load(SeqCst));
        let response = request_sync_with_timeout_on(
            scheduler,
            "GET",
            "http://timer.test/fast",
            &[],
            None,
            &config,
            60_000,
        )
        .unwrap();
        assert_eq!(response.body, b"ok");
        assert_eq!(scheduler.armed.load(SeqCst), armed + 1);
        assert_eq!(scheduler.cancelled.load(SeqCst), cancelled + 1);
        server.join().unwrap();

        // No deadline, no timer.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = crate::FetchConfig::default();
        route(&listener, &mut config);
        let server = serve(listener, Duration::ZERO);
        let armed = scheduler.armed.load(SeqCst);
        request_sync_with_timeout_on(
            scheduler,
            "GET",
            "http://timer.test/none",
            &[],
            None,
            &config,
            0,
        )
        .unwrap();
        assert_eq!(scheduler.armed.load(SeqCst), armed);
        server.join().unwrap();
    }

    #[test]
    fn same_origin_module_redirect_preserves_logical_query() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    let end = line == "\r\n";
                    request.push_str(&line);
                    if end {
                        break;
                    }
                }
                if index == 0 {
                    stream
                        .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://origin.test:80/dir/final.js?case=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .unwrap();
                }
                requests.push(request);
            }
            requests
        });

        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("origin.test", 80, address).unwrap();
        let module = crate::load_module_resource_same_origin_with_config(
            "http://origin.test/start.js?case=1",
            &config,
        )
        .unwrap();

        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /start.js?case=1 HTTP/1.1\r\n"));
        assert!(requests[0].contains("\r\nHost: origin.test\r\n"));
        assert!(requests[1].starts_with("GET /dir/final.js?case=1 HTTP/1.1\r\n"));
        assert_eq!(module.url, "http://origin.test/dir/final.js?case=1");
        assert_eq!(module.bytes, b"ok");
    }

    #[test]
    fn same_origin_module_redirect_rejects_cross_origin_hop_before_requesting_it() {
        use std::time::Instant;

        fn read_request(stream: &mut std::net::TcpStream) -> String {
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                request.push_str(&line);
                if end {
                    break;
                }
            }
            request
        }

        let origin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        origin_listener.set_nonblocking(true).unwrap();
        let origin_address = origin_listener.local_addr().unwrap();
        let outside_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        outside_listener.set_nonblocking(true).unwrap();
        let outside_address = outside_listener.local_addr().unwrap();

        let origin_server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut requests = Vec::new();
            while Instant::now() < deadline && requests.len() < 2 {
                match origin_listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        if requests.is_empty() {
                            stream
                                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://outside.test:8000/escape.js?case=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .unwrap();
                        } else {
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                                .unwrap();
                        }
                        requests.push(request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept origin fixture request: {error}"),
                }
            }
            requests
        });
        let outside_server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match outside_listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        stream
                            .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://origin.test:8000/final.js?case=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .unwrap();
                        return Some(request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept outside fixture request: {error}"),
                }
            }
        });

        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config
            .set_route("origin.test", 8000, origin_address)
            .unwrap();
        config
            .set_route("outside.test", 8000, outside_address)
            .unwrap();
        let error = match crate::load_module_resource_same_origin_with_config(
            "http://origin.test:8000/start.js?case=1",
            &config,
        ) {
            Ok(_) => panic!("cross-origin redirect chain unexpectedly succeeded"),
            Err(error) => error,
        };

        let origin_requests = origin_server.join().unwrap();
        let outside_request = outside_server.join().unwrap();
        assert!(
            error.contains("same-origin policy"),
            "unexpected error: {error}"
        );
        assert_eq!(
            origin_requests.len(),
            1,
            "the redirect must not return to origin"
        );
        assert!(origin_requests[0].starts_with("GET /start.js?case=1 HTTP/1.1\r\n"));
        assert!(
            outside_request.is_none(),
            "cross-origin redirect hop was sent: {outside_request:?}"
        );
    }

    #[test]
    fn strict_fixture_routes_block_unconfigured_redirect_hops() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_http_once(
            listener,
            "HTTP/1.1 302 Found\r\nLocation: http://outside.test:8000/final.js\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let mut config = crate::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("origin.test", 8000, address).unwrap();

        let error = request_with_config(
            "GET",
            "http://origin.test:8000/start.js",
            &[],
            None,
            &config,
        )
        .err()
        .expect("unconfigured redirect must be rejected");
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /start.js HTTP/1.1\r\n"));
        assert!(
            error.contains("no fixture route configured for outside.test:8000"),
            "unexpected redirect error: {error}"
        );
    }

    fn serve_tls_requests(
        listener: TcpListener,
        certificate: Vec<u8>,
        private_key: Vec<u8>,
        count: usize,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for _ in 0..count {
                let (tcp, _) = listener.accept().unwrap();
                tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let Ok(mut tls) = lumen_tls::TlsStream::accept(tcp, &certificate, &private_key)
                else {
                    continue;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while request.len() < 16 * 1024 {
                    match tls.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => {
                            request.push(byte[0]);
                            if request.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        })
    }

    fn test_ca_and_server_certificate() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut ca_params = rcgen::CertificateParams::new(vec!["test-ca".into()]).unwrap();
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Bitnest WPT fixture CA");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut leaf_params = rcgen::CertificateParams::new(vec!["trusted.test".into()]).unwrap();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "trusted.test");
        leaf_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
        ];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        (
            ca.pem().into_bytes(),
            leaf.pem().into_bytes(),
            leaf_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn extra_roots_are_route_scoped_and_hostname_verification_is_preserved() {
        let (certificate_authority, server_certificate, private_key) =
            test_ca_and_server_certificate();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_tls_requests(listener, server_certificate, private_key, 3);

        let mut trusted = crate::FetchConfig::default();
        trusted
            .set_route_with_extra_roots(
                "trusted.test",
                8443,
                address,
                certificate_authority.clone(),
            )
            .unwrap();
        let trusted_result =
            request_with_config("GET", "https://trusted.test:8443/", &[], None, &trusted);

        let mut without_extra_root = crate::FetchConfig::default();
        without_extra_root
            .set_route("trusted.test", 8443, address)
            .unwrap();
        let untrusted_result = request_with_config(
            "GET",
            "https://trusted.test:8443/",
            &[],
            None,
            &without_extra_root,
        );

        let mut wrong_hostname = crate::FetchConfig::default();
        wrong_hostname
            .set_route_with_extra_roots("wrong.test", 8443, address, certificate_authority)
            .unwrap();
        let wrong_hostname_result = request_with_config(
            "GET",
            "https://wrong.test:8443/",
            &[],
            None,
            &wrong_hostname,
        );

        server.join().unwrap();
        assert_eq!(trusted_result.unwrap().body, b"ok");
        assert!(
            untrusted_result.is_err(),
            "extra route CA leaked into default trust"
        );
        assert!(
            wrong_hostname_result.is_err(),
            "TLS hostname verification was bypassed"
        );
    }

    #[test]
    #[ignore = "requires external network and a system OpenSSL trust store"]
    fn https_uses_verified_tls() {
        let response = request("GET", "https://example.com/", &[], None).unwrap();
        assert_eq!(response.status, 200);
    }
}
