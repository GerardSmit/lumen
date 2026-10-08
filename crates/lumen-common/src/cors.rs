//! Language- and transport-independent Fetch CORS policy primitives.
//!
//! Adapters own URL parsing, origin acquisition, cancellation, and network I/O. They call this
//! module on request and response heads before exposing a response body or sending an unsafe
//! cross-origin request.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Cors,
    NoCors,
    SameOrigin,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Credentials {
    Omit,
    SameOrigin,
    Include,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Redirect {
    Follow,
    Error,
    Manual,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseType {
    Basic,
    Cors,
    Opaque,
    OpaqueRedirect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyError {
    InvalidMode,
    InvalidCredentials,
    InvalidRedirect,
    SameOrigin,
    NoCorsMethod,
    NoCorsRedirect,
    Preflight,
    Cors,
    Redirect,
    TooManyRedirects,
    InvalidMethod,
    InvalidHeader,
    InvalidUrl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestHead {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilteredResponse {
    pub kind: ResponseType,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct FetchPolicy {
    origin: String,
    cors_origin: String,
    mode: Mode,
    credentials: Credentials,
    redirect: Redirect,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    current_url_origin: String,
    redirected: bool,
    cors_tainted: bool,
    redirect_count: usize,
    force_preflight: bool,
}

impl FetchPolicy {
    /// A user-agent CSP report: fixed method/MIME/credentials, no CORS handshake,
    /// and no redirects. This entrypoint does not change author fetch validation.
    pub fn new_policy_report(origin: &str, url: &str, body: Vec<u8>) -> Result<Self, PolicyError> {
        let target = crate::url::parse_url(url, None).ok_or(PolicyError::InvalidUrl)?;
        if !matches!(target.scheme.as_str(), "http" | "https") { return Err(PolicyError::InvalidUrl); }
        let mut policy = Self::new(origin,"POST",url,&target.origin(),
            vec![("content-type".into(),"application/csp-report".into())],Some(body),
            Mode::Cors,Credentials::SameOrigin,Redirect::Error)?;
        policy.mode = Mode::NoCors;
        set_header(&mut policy.headers,"origin",origin);
        Ok(policy)
    }
    pub fn new(
        origin: &str,
        method: &str,
        url: &str,
        url_origin: &str,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
        mode: Mode,
        credentials: Credentials,
        redirect: Redirect,
    ) -> Result<Self, PolicyError> {
        let parsed_url = crate::url::parse_url(url, None).ok_or(PolicyError::InvalidUrl)?;
        if parsed_url.origin() != url_origin
            || !parsed_url.username.is_empty()
            || !parsed_url.password.is_empty()
        {
            return Err(PolicyError::InvalidUrl);
        }
        let method = if ["GET", "HEAD", "POST"]
            .iter()
            .any(|known| method.eq_ignore_ascii_case(known))
        {
            method.to_ascii_uppercase()
        } else {
            method.to_string()
        };
        if method.is_empty()
            || !method.bytes().all(is_http_token_byte)
            || ["CONNECT", "TRACE", "TRACK"]
                .iter()
                .any(|blocked| method.eq_ignore_ascii_case(blocked))
        {
            return Err(PolicyError::InvalidMethod);
        }
        if headers
            .iter()
            .any(|(name, value)| !valid_header(name, value))
        {
            return Err(PolicyError::InvalidHeader);
        }
        if mode == Mode::SameOrigin && (origin == "null" || origin != url_origin) {
            return Err(PolicyError::SameOrigin);
        }
        if mode == Mode::NoCors {
            if !is_safelisted_method(&method) {
                return Err(PolicyError::NoCorsMethod);
            }
            if redirect != Redirect::Follow {
                return Err(PolicyError::NoCorsRedirect);
            }
        }
        let mut combined = Vec::<(String, String)>::new();
        for (name, value) in headers {
            let name = name.to_ascii_lowercase();
            if let Some((_, previous)) = combined.iter_mut().find(|(key, _)| key == &name) {
                previous.push_str(", ");
                previous.push_str(&value);
            } else {
                combined.push((name, value));
            }
        }
        let headers = if mode == Mode::NoCors {
            combined
                .into_iter()
                .filter(|(name, value)| is_no_cors_safelisted_header(name, value))
                .collect()
        } else {
            combined
                .into_iter()
                .filter(|(name, _)| !is_forbidden_request_header(name))
                .collect()
        };
        let cors_tainted = origin == "null";
        Ok(Self {
            origin: origin.to_string(),
            cors_origin: if cors_tainted {
                "null".into()
            } else {
                origin.to_string()
            },
            mode,
            credentials,
            redirect,
            method,
            url: url.to_string(),
            headers,
            body,
            current_url_origin: url_origin.to_string(),
            redirected: false,
            cors_tainted,
            redirect_count: 0,
            force_preflight: false,
        })
    }

    pub fn current_url(&self) -> &str {
        &self.url
    }
    /// Credentials are selected from the trusted request policy at each redirect hop.
    pub fn credentials_allowed(&self) -> bool {
        self.credentials == Credentials::Include
            || (self.credentials == Credentials::SameOrigin
                && self.current_url_origin == self.origin && !self.cors_tainted)
    }
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn is_redirected(&self) -> bool {
        self.redirected
    }
    pub fn needs_preflight(&self) -> bool {
        self.mode == Mode::Cors
            && (self.current_url_origin != self.origin || self.cors_tainted)
            && (self.force_preflight || !is_safelisted_method(&self.method)
                || !cors_unsafe_header_names(&self.headers).is_empty())
    }
    /// XHR upload listeners require a preflight even for a safelisted request.
    pub fn set_force_preflight(&mut self, enabled: bool) {
        self.force_preflight = enabled;
    }
    pub fn preflight_request(&self) -> Option<RequestHead> {
        if !self.needs_preflight() {
            return None;
        }
        let unsafe_names = cors_unsafe_header_names(&self.headers);
        let mut headers = vec![
            ("accept".to_string(), "*/*".to_string()),
            ("origin".to_string(), self.cors_origin.clone()),
            (
                "access-control-request-method".to_string(),
                self.method.clone(),
            ),
        ];
        if !unsafe_names.is_empty() {
            headers.push((
                "access-control-request-headers".to_string(),
                unsafe_names.join(","),
            ));
        }
        Some(RequestHead {
            method: "OPTIONS".into(),
            url: self.url.clone(),
            headers,
            body: None,
        })
    }
    pub fn validate_preflight(
        &self,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<(), PolicyError> {
        if !(200..300).contains(&status)
            || !cors_check(headers, &self.cors_origin, self.credentials)
        {
            return Err(PolicyError::Preflight);
        }
        let methods = comma_tokens(headers, "access-control-allow-methods");
        if !methods
            .iter()
            .any(|value| value.eq_ignore_ascii_case(&self.method))
            && !(self.credentials != Credentials::Include
                && methods.iter().any(|value| value == "*"))
            && !is_safelisted_method(&self.method)
        {
            return Err(PolicyError::Preflight);
        }
        let allowed = comma_tokens(headers, "access-control-allow-headers");
        let wildcard =
            self.credentials != Credentials::Include && allowed.iter().any(|value| value == "*");
        for name in cors_unsafe_header_names(&self.headers) {
            if name.eq_ignore_ascii_case("authorization")
                && !allowed
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(&name))
            {
                return Err(PolicyError::Preflight);
            }
            if !allowed
                .iter()
                .any(|value| value.eq_ignore_ascii_case(&name))
                && !wildcard
            {
                return Err(PolicyError::Preflight);
            }
        }
        Ok(())
    }
    pub fn actual_body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }
    pub fn actual_request(&self) -> RequestHead {
        let mut head = self.actual_request_head();
        head.body = self.body.clone();
        head
    }
    /// The next request without its body, for callers that only borrow the body from the policy.
    pub fn actual_request_head(&self) -> RequestHead {
        let mut headers = self.headers.clone();
        if self.mode == Mode::Cors && (self.current_url_origin != self.origin || self.cors_tainted)
        {
            set_header(&mut headers, "origin", &self.cors_origin);
        }
        RequestHead {
            method: self.method.clone(),
            url: self.url.clone(),
            headers,
            body: None,
        }
    }
    /// Fetch's TAO check uses the current hop's response tainting and serialized
    /// request origin. The transport retains failure across redirect hops.
    pub fn timing_allow_response(&self, headers: &[(String, String)]) -> bool {
        let cross = self.cors_tainted || self.current_url_origin != self.origin;
        if !cross { return true; }
        headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("timing-allow-origin"))
            .flat_map(|(_, value)| value.split(','))
            .map(|value|value.trim_matches(|c|matches!(c,' '| '\t')))
            .any(|value|value == "*" || value == self.cors_origin)
    }
    /// Validate a response head and, when it is a followed redirect, update the next hop.
    /// Return `true` when the adapter should issue the next request.
    pub fn response_head(
        &mut self,
        status: u16,
        headers: &[(String, String)],
        location: Option<&str>,
        next_url: Option<(&str, &str)>,
    ) -> Result<bool, PolicyError> {
        let cross = self.mode == Mode::Cors
            && (self.current_url_origin != self.origin || self.cors_tainted);
        if cross && !cors_check(headers, &self.cors_origin, self.credentials) {
            return Err(PolicyError::Cors);
        }
        let is_redirect = matches!(status, 301 | 302 | 303 | 307 | 308) && location.is_some();
        if !is_redirect {
            return Ok(false);
        }
        match self.redirect {
            Redirect::Error => return Err(PolicyError::Redirect),
            Redirect::Manual => return Ok(false),
            Redirect::Follow => {}
        }
        if self.redirect_count >= 20 {
            return Err(PolicyError::TooManyRedirects);
        }
        let (next_url, next_origin) = next_url.ok_or(PolicyError::Redirect)?;
        if self.mode == Mode::SameOrigin && (next_origin == "null" || next_origin != self.origin) {
            return Err(PolicyError::SameOrigin);
        }
        if let Ok(parsed) = crate::url::parse(next_url, Some(&self.url)) {
            if !parsed.username.is_empty() || !parsed.password.is_empty() {
                return Err(PolicyError::Redirect);
            }
        }
        if (status == 303 && self.method != "GET" && self.method != "HEAD")
            || ((status == 301 || status == 302) && self.method == "POST")
        {
            self.method = "GET".into();
            self.body = None;
            self.headers
                .retain(|(name, _)| !is_request_body_header(name));
        }
        if self.current_url_origin != next_origin && self.current_url_origin != self.origin {
            self.cors_tainted = true;
            self.cors_origin = "null".into();
        }
        if self.current_url_origin != next_origin {
            self.headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case("authorization")
                    && !name.eq_ignore_ascii_case("proxy-authorization")
                    && !name.eq_ignore_ascii_case("cookie")
            });
        }
        self.url = next_url.to_string();
        self.current_url_origin = next_origin.to_string();
        self.redirected = true;
        self.redirect_count += 1;
        Ok(true)
    }
    pub fn filter_response(&self, headers: &[(String, String)]) -> FilteredResponse {
        let cross = self.cors_tainted || self.current_url_origin != self.origin;
        if self.mode == Mode::NoCors && cross {
            return FilteredResponse {
                kind: ResponseType::Opaque,
                headers: Vec::new(),
            };
        }
        let kind = if cross && self.mode == Mode::Cors {
            ResponseType::Cors
        } else {
            ResponseType::Basic
        };
        let exposed = comma_tokens(headers, "access-control-expose-headers");
        let wildcard =
            self.credentials != Credentials::Include && exposed.iter().any(|value| value == "*");
        let visible = headers
            .iter()
            .filter(|(name, _)| {
                !is_forbidden_response_header(name)
                    && (kind == ResponseType::Basic
                        || is_cors_safelisted_response_header(name)
                        || wildcard
                        || exposed.iter().any(|value| value.eq_ignore_ascii_case(name)))
            })
            .cloned()
            .collect();
        FilteredResponse {
            kind,
            headers: visible,
        }
    }
    pub fn redirect_response_type(&self) -> ResponseType {
        ResponseType::OpaqueRedirect
    }
}

pub fn is_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}
fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}
fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(is_http_token_byte)
        && value.chars().all(|character| character as u32 <= 0xff)
        && !value.bytes().any(|byte| {
            byte == 0
                || byte == b'\r'
                || byte == b'\n'
                || (byte < 0x20 && byte != b'\t')
                || byte == 0x7f
        })
}
pub fn is_forbidden_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) || lower.starts_with("proxy-")
        || lower.starts_with("sec-")
}
pub fn cors_unsafe_header_names(headers: &[(String, String)]) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut potentially_unsafe = Vec::new();
    let mut size = 0usize;
    for (name, value) in headers {
        if is_forbidden_request_header(name) {
            continue;
        }
        if is_cors_safelisted_request_header(name, value) {
            potentially_unsafe.push(name.to_ascii_lowercase());
            size = size.saturating_add(value.len());
        } else {
            unsafe_names.push(name.to_ascii_lowercase());
        }
    }
    if size > 1024 {
        unsafe_names.extend(potentially_unsafe);
    }
    unsafe_names.sort();
    unsafe_names.dedup();
    unsafe_names
}
pub fn is_no_cors_safelisted_header(name: &str, value: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "content-language" | "content-type"
    ) && is_cors_safelisted_request_header(name, value)
}
fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "accept" => !has_cors_unsafe_byte(value),
        "accept-language" | "content-language" => value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
        }),
        "content-type" => {
            !has_cors_unsafe_byte(value)
                && matches!(
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                        .as_str(),
                    "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
                )
        }
        "range" => {
            let Some(value) = value.strip_prefix("bytes=") else {
                return false;
            };
            let Some((start, end)) = value.split_once('-') else {
                return false;
            };
            !start.is_empty()
                && start.bytes().all(|b| b.is_ascii_digit())
                && (end.is_empty()
                    || (end.bytes().all(|b| b.is_ascii_digit()) && decimal_le(start, end)))
        }
        _ => false,
    }
}
fn decimal_le(left: &str, right: &str) -> bool {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len() < right.len() || (left.len() == right.len() && left <= right)
}
fn has_cors_unsafe_byte(value: &str) -> bool {
    value.bytes().any(|byte| {
        (byte < 0x20 && byte != b'\t')
            || matches!(
                byte,
                b'"' | b'('
                    | b')'
                    | b':'
                    | b'<'
                    | b'>'
                    | b'?'
                    | b'@'
                    | b'['
                    | b'\\'
                    | b']'
                    | b'{'
                    | b'}'
                    | 0x7f
            )
    })
}
fn is_request_body_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-encoding" | "content-language" | "content-location" | "content-type"
    )
}
fn is_forbidden_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "set-cookie" | "set-cookie2"
    )
}
fn is_cors_safelisted_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cache-control"
            | "content-language"
            | "content-length"
            | "content-type"
            | "expires"
            | "last-modified"
            | "pragma"
    )
}
fn header_values<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> impl Iterator<Item = &'a str> + 'a {
    let name = name.to_ascii_lowercase();
    headers
        .iter()
        .filter(move |(key, _)| key.eq_ignore_ascii_case(&name))
        .map(|(_, value)| value.as_str())
}
fn single_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut values = header_values(headers, name);
    let value = values.next()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}
fn comma_tokens(headers: &[(String, String)], name: &str) -> Vec<String> {
    header_values(headers, name)
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}
pub fn cors_check(headers: &[(String, String)], origin: &str, credentials: Credentials) -> bool {
    let Some(allowed) = single_header(headers, "access-control-allow-origin") else {
        return false;
    };
    let allowed = allowed.trim();
    if credentials != Credentials::Include && allowed == "*" {
        return true;
    }
    if allowed != origin {
        return false;
    }
    credentials != Credentials::Include
        || single_header(headers, "access-control-allow-credentials")
            .is_some_and(|value| value.trim() == "true")
}
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

extern crate alloc;

#[cfg(test)]
#[test]
fn upload_listeners_force_preflight_only_across_origins() {
    let mut cross = FetchPolicy::new("https://page.test", "POST", "https://upload.test/",
        "https://upload.test", Vec::new(), Some(alloc::vec![65]), Mode::Cors,
        Credentials::SameOrigin, Redirect::Follow).unwrap();
    assert!(!cross.needs_preflight());
    cross.set_force_preflight(true);
    assert!(cross.needs_preflight());
    let head = cross.preflight_request().unwrap();
    assert_eq!(head.method, "OPTIONS");
    assert!(head.body.is_none());
    assert!(!head.headers.iter().any(|(name, _)| name == "access-control-request-headers"));
    let mut same = FetchPolicy::new("https://page.test", "POST", "https://page.test/",
        "https://page.test", Vec::new(), Some(alloc::vec![65]), Mode::Cors,
        Credentials::SameOrigin, Redirect::Follow).unwrap();
    same.set_force_preflight(true);
    assert!(!same.needs_preflight());
}

#[cfg(test)]
mod policy_report_tests {
    use super::*;
    #[test]
    fn ua_reports_preserve_mime_omit_cross_origin_credentials_and_reject_redirects() {
        let mut report=FetchPolicy::new_policy_report("https://example.test","https://other.test/report",b"{}".to_vec()).unwrap();
        assert!(report.preflight_request().is_none());assert!(!report.credentials_allowed());
        let head=report.actual_request();assert_eq!(head.method,"POST");
        assert!(head.headers.iter().any(|(name,value)|name=="content-type"&&value=="application/csp-report"));
        assert!(head.headers.iter().any(|(name,value)|name=="origin"&&value=="https://example.test"));
        assert_eq!(report.response_head(302,&[],Some("https://example.test/redirected"),Some(("https://example.test/redirected","https://example.test"))),Err(PolicyError::Redirect));
        assert!(FetchPolicy::new_policy_report("https://example.test","https://example.test/report",Vec::new()).unwrap().credentials_allowed());
        assert!(matches!(FetchPolicy::new("https://example.test","POST","https://other.test/report","https://other.test",Vec::new(),None,Mode::NoCors,Credentials::SameOrigin,Redirect::Error),Err(PolicyError::NoCorsRedirect)));
    }
}

#[cfg(test)]
#[test]
fn specification_stylesheet_timing_access_is_independent_of_cors_and_redirect_tainting() {
    let mut policy=FetchPolicy::new("https://page.test","GET","https://page.test/root.css","https://page.test",Vec::new(),None,
        Mode::NoCors,Credentials::Include,Redirect::Follow).unwrap();
    assert!(policy.timing_allow_response(&[]));
    policy.response_head(302,&[],Some("https://other.test/root.css"),Some(("https://other.test/root.css","https://other.test"))).unwrap();
    assert!(!policy.timing_allow_response(&[]));
    assert!(policy.timing_allow_response(&alloc::vec![("Timing-Allow-Origin".into(),"*".into())]),"TAO wildcard permits credentialed responses");
    assert!(policy.timing_allow_response(&alloc::vec![("Timing-Allow-Origin".into(),"https://page.test".into())]));
    policy.response_head(302,&[],Some("https://third.test/next.css"),Some(("https://third.test/next.css","https://third.test"))).unwrap();
    assert!(!policy.timing_allow_response(&alloc::vec![("Timing-Allow-Origin".into(),"https://page.test".into())]),"redirect-tainted origin is null");
    assert!(policy.timing_allow_response(&alloc::vec![("Timing-Allow-Origin".into(),"null".into())]));
}
