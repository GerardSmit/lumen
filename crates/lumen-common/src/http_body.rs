//! Transport-independent incremental HTTP/1 response framing.
//! Incomplete framing lines survive socket wakeups; bodies need no full buffer.
extern crate alloc;
use alloc::{string::String, vec::Vec};
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Thread-safe progress for the request body bytes accepted by the transport.
/// The total is fixed when the request starts; a missing total is represented as zero.
#[derive(Debug)]
pub struct UploadProgress {
    loaded: AtomicU64,
    total: AtomicU64,
    complete: AtomicBool,
}

impl UploadProgress {
    pub fn new(total: Option<u64>) -> Self {
        Self {
            loaded: AtomicU64::new(0),
            total: AtomicU64::new(total.unwrap_or(0)),
            complete: AtomicBool::new(false),
        }
    }

    pub fn loaded(&self) -> u64 {
        self.loaded.load(Ordering::Acquire)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }

    pub fn complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// Add successfully accepted body bytes. Redirect retransmissions after the
    /// first completed upload are ignored; the exposed count is capped at total.
    pub fn record_written(&self, count: usize) {
        if self.complete() || count == 0 {
            return;
        }
        let total = self.total();
        let mut loaded = self.loaded.load(Ordering::Relaxed);
        loop {
            if self.complete() {
                return;
            }
            let next = loaded.saturating_add(count as u64).min(total);
            match self.loaded.compare_exchange_weak(
                loaded,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => loaded = actual,
            }
        }
    }

    /// Freeze progress after the first request body's write and flush finish.
    pub fn mark_complete(&self) {
        if self.complete() {
            return;
        }
        self.complete.store(true, Ordering::Release);
    }
}

/// Bounded binary request used by synchronous host bridges. A length-prefixed
/// format keeps request and response bodies binary-safe across the wasm bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: String,
    pub credentials: String,
    pub redirect: String,
    pub force_preflight: bool,
    /// XHR timeout in milliseconds; zero means no per-request timeout.
    pub timeout_ms: u32,
    pub origin: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

const SYNC_REQUEST_MAGIC: &[u8; 4] = b"XHR1";
const SYNC_RESPONSE_MAGIC: &[u8; 4] = b"XHS1";
const SYNC_FIELD_LIMIT: usize = 1 << 20;
const SYNC_HEADER_LIMIT: usize = 8192;
const SYNC_HEADER_BYTES: usize = 64 << 10;
const SYNC_BODY_LIMIT: usize = 32 << 20;

impl SyncHttpRequest {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_REQUEST_MAGIC);
        put_string16(&mut out, &self.method)?;
        put_string32(&mut out, &self.url)?;
        put_headers(&mut out, &self.headers)?;
        put_string16(&mut out, &self.mode)?;
        put_string16(&mut out, &self.credentials)?;
        put_string16(&mut out, &self.redirect)?;
        out.push(u8::from(self.force_preflight));
        out.extend_from_slice(&self.timeout_ms.to_le_bytes());
        match &self.origin {
            None => out.push(0),
            Some(origin) => {
                out.push(1);
                put_string32(&mut out, origin)?;
            }
        }
        match &self.body {
            None => out.push(0),
            Some(body) => {
                if body.len() > SYNC_BODY_LIMIT {
                    return Err(Error("synchronous HTTP request body is too large"));
                }
                out.push(1);
                put_u32(&mut out, body.len())?;
                out.extend_from_slice(body);
            }
        }
        Ok(out)
    }

    pub fn decode(input: &[u8]) -> Result<Self, Error> {
        let mut reader = SyncReader::new(input, SYNC_REQUEST_MAGIC)?;
        let method = reader.string16()?;
        let url = reader.string32()?;
        let headers = reader.headers()?;
        let mode = reader.string16()?;
        let credentials = reader.string16()?;
        let redirect = reader.string16()?;
        let force_preflight = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(Error("invalid synchronous HTTP preflight marker")),
        };
        let timeout_ms = u32::from_le_bytes(reader.take(4)?.try_into().unwrap());
        let origin = match reader.byte()? {
            0 => None,
            1 => Some(reader.string32()?),
            _ => return Err(Error("invalid synchronous HTTP origin marker")),
        };
        let body = match reader.byte()? {
            0 => None,
            1 => Some(reader.bytes32(SYNC_BODY_LIMIT)?.to_vec()),
            _ => return Err(Error("invalid synchronous HTTP request body marker")),
        };
        reader.finish()?;
        Ok(Self {
            method,
            url,
            headers,
            body,
            mode,
            credentials,
            redirect,
            force_preflight,
            timeout_ms,
            origin,
        })
    }
}

impl SyncHttpResponse {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.body.len() > SYNC_BODY_LIMIT {
            return Err(Error("synchronous HTTP response body is too large"));
        }
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_RESPONSE_MAGIC);
        out.extend_from_slice(&self.status.to_le_bytes());
        put_string16(&mut out, &self.status_text)?;
        put_string32(&mut out, &self.url)?;
        put_headers(&mut out, &self.headers)?;
        put_u32(&mut out, self.body.len())?;
        out.extend_from_slice(&self.body);
        Ok(out)
    }

    pub fn decode(input: &[u8]) -> Result<Self, Error> {
        let mut reader = SyncReader::new(input, SYNC_RESPONSE_MAGIC)?;
        let status = u16::from_le_bytes(reader.take(2)?.try_into().unwrap());
        let status_text = reader.string16()?;
        let url = reader.string32()?;
        let headers = reader.headers()?;
        let body = reader.bytes32(SYNC_BODY_LIMIT)?.to_vec();
        reader.finish()?;
        Ok(Self {
            status,
            status_text,
            url,
            headers,
            body,
        })
    }
}

fn put_string16(out: &mut Vec<u8>, value: &str) -> Result<(), Error> {
    let length =
        u16::try_from(value.len()).map_err(|_| Error("synchronous HTTP field is too large"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_string32(out: &mut Vec<u8>, value: &str) -> Result<(), Error> {
    if value.len() > SYNC_FIELD_LIMIT {
        return Err(Error("synchronous HTTP field is too large"));
    }
    put_u32(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, length: usize) -> Result<(), Error> {
    let length = u32::try_from(length).map_err(|_| Error("synchronous HTTP field is too large"))?;
    out.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

fn put_headers(out: &mut Vec<u8>, headers: &[(String, String)]) -> Result<(), Error> {
    if headers.len() > SYNC_HEADER_LIMIT {
        return Err(Error("too many synchronous HTTP headers"));
    }
    let size = headers
        .iter()
        .try_fold(0usize, |size, (name, value)| {
            size.checked_add(name.len())?.checked_add(value.len())
        })
        .ok_or(Error("synchronous HTTP header size overflow"))?;
    if size > SYNC_HEADER_BYTES {
        return Err(Error("synchronous HTTP headers exceed the 64 KiB limit"));
    }
    out.extend_from_slice(&(headers.len() as u16).to_le_bytes());
    for (name, value) in headers {
        put_string16(out, name)?;
        put_string32(out, value)?;
    }
    Ok(())
}

struct SyncReader<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> SyncReader<'a> {
    fn new(input: &'a [u8], magic: &[u8; 4]) -> Result<Self, Error> {
        if input.get(..4) != Some(magic) {
            return Err(Error("invalid synchronous HTTP message"));
        }
        Ok(Self { input, at: 4 })
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(length)
            .ok_or(Error("synchronous HTTP length overflow"))?;
        let bytes = self
            .input
            .get(self.at..end)
            .ok_or(Error("truncated synchronous HTTP message"))?;
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn string16(&mut self) -> Result<String, Error> {
        let length = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        self.string(length)
    }

    fn string32(&mut self) -> Result<String, Error> {
        let length = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        if length > SYNC_FIELD_LIMIT {
            return Err(Error("synchronous HTTP field is too large"));
        }
        self.string(length)
    }

    fn string(&mut self, length: usize) -> Result<String, Error> {
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| Error("invalid UTF-8 in synchronous HTTP message"))
    }

    fn bytes32(&mut self, limit: usize) -> Result<&'a [u8], Error> {
        let length = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        if length > limit {
            return Err(Error("synchronous HTTP body is too large"));
        }
        self.take(length)
    }

    fn headers(&mut self) -> Result<Vec<(String, String)>, Error> {
        let count = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        if count > SYNC_HEADER_LIMIT {
            return Err(Error("too many synchronous HTTP headers"));
        }
        let mut headers = Vec::with_capacity(count);
        let mut size = 0usize;
        for _ in 0..count {
            let name = self.string16()?;
            let value = self.string32()?;
            size = size.saturating_add(name.len()).saturating_add(value.len());
            if size > SYNC_HEADER_BYTES {
                return Err(Error("synchronous HTTP headers exceed the 64 KiB limit"));
            }
            headers.push((name, value));
        }
        Ok(headers)
    }

    fn finish(&self) -> Result<(), Error> {
        if self.at == self.input.len() {
            Ok(())
        } else {
            Err(Error("trailing synchronous HTTP data"))
        }
    }
}

#[cfg(test)]
#[test]
fn upload_progress_never_fabricates_writes_and_freezes_after_completion() {
    let progress = UploadProgress::new(Some(10));
    progress.record_written(3);
    assert_eq!(progress.loaded(), 3);
    progress.mark_complete();
    assert_eq!(progress.loaded(), 3);
    progress.record_written(20);
    assert_eq!(progress.loaded(), 3);
    assert!(progress.complete());
    let progress = UploadProgress::new(Some(10));
    progress.record_written(usize::MAX);
    assert_eq!(progress.loaded(), 10);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Empty,
    Length(u64),
    Chunked,
    Eof,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl core::error::Error for Error {}

pub fn framing(method: &str, status: u16, headers: &[(String, String)]) -> Result<Framing, Error> {
    if method.eq_ignore_ascii_case("HEAD")
        || matches!(status, 204 | 205 | 304)
        || (100..200).contains(&status)
    {
        return Ok(Framing::Empty);
    }
    let transfer = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    if !transfer.is_empty() {
        if transfer.len() != 1 || !transfer[0].eq_ignore_ascii_case("chunked") {
            return Err(Error("unsupported HTTP transfer coding"));
        }
        return Ok(Framing::Chunked);
    }
    let mut length = None;
    for value in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .flat_map(|(_, value)| value.split(','))
    {
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Error("invalid HTTP content length"));
        }
        let parsed = value
            .parse::<u64>()
            .map_err(|_| Error("HTTP content length overflow"))?;
        if length.is_some_and(|previous| previous != parsed) {
            return Err(Error("conflicting HTTP content lengths"));
        }
        length = Some(parsed);
    }
    Ok(length.map_or(Framing::Eof, Framing::Length))
}

#[derive(Debug)]
enum State {
    Data(u64),
    Eof,
    Size,
    Terminator(usize),
    Trailers,
    Done,
    Failed,
}
pub struct Step {
    pub consumed: usize,
    pub chunk: Option<Vec<u8>>,
    pub done: bool,
}
pub struct Decoder {
    framing: Framing,
    state: State,
    line: Vec<u8>,
    trailer_bytes: usize,
    received: u64,
    limit: u64,
}
impl Decoder {
    pub fn new(framing: Framing, limit: u64) -> Result<Self, Error> {
        if matches!(framing, Framing::Length(length) if length > limit) {
            return Err(Error("HTTP body exceeds configured limit"));
        }
        let state = match framing {
            Framing::Empty | Framing::Length(0) => State::Done,
            Framing::Length(n) => State::Data(n),
            Framing::Chunked => State::Size,
            Framing::Eof => State::Eof,
        };
        Ok(Self {
            framing,
            state,
            line: Vec::new(),
            trailer_bytes: 0,
            received: 0,
            limit,
        })
    }
    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }
    pub fn has_body(&self) -> bool {
        self.framing != Framing::Empty
    }
    /// Consume supplied transport bytes, returning a body chunk as soon as it
    /// exists. Unconsumed bytes belong to the transport, including a next message.
    pub fn decode(&mut self, input: &[u8], eof: bool, maximum: usize) -> Result<Step, Error> {
        if maximum == 0 {
            return Err(Error("HTTP chunk size must be positive"));
        }
        let result = self.decode_inner(input, eof, maximum);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }
    fn decode_inner(&mut self, input: &[u8], eof: bool, maximum: usize) -> Result<Step, Error> {
        let mut at = 0;
        loop {
            match self.state {
                State::Done => {
                    return Ok(Step {
                        consumed: at,
                        chunk: None,
                        done: true,
                    });
                }
                State::Failed => return Err(Error("HTTP body decoder has failed")),
                State::Data(_) | State::Eof => {
                    let left = match self.state {
                        State::Data(n) => n,
                        _ => self.limit.saturating_sub(self.received),
                    };
                    if at == input.len() {
                        if eof {
                            if matches!(self.state, State::Data(_)) {
                                return Err(Error("truncated HTTP body"));
                            }
                            self.state = State::Done;
                            continue;
                        }
                        return Ok(Step {
                            consumed: at,
                            chunk: None,
                            done: false,
                        });
                    }
                    if left == 0 {
                        return Err(Error("HTTP body exceeds configured limit"));
                    }
                    let count = maximum
                        .min(input.len() - at)
                        .min(left.min(usize::MAX as u64) as usize);
                    let chunk = input[at..at + count].to_vec();
                    self.received += count as u64;
                    at += count;
                    if let State::Data(remaining) = &mut self.state {
                        *remaining -= count as u64;
                        if *remaining == 0 {
                            self.state = if self.framing == Framing::Chunked {
                                State::Terminator(0)
                            } else {
                                State::Done
                            };
                        }
                    }
                    return Ok(Step {
                        consumed: at,
                        chunk: Some(chunk),
                        done: self.is_done(),
                    });
                }
                State::Terminator(ref mut offset) => {
                    while at < input.len() && *offset < 2 {
                        if input[at] != b"\r\n"[*offset] {
                            return Err(Error("invalid HTTP chunk terminator"));
                        }
                        at += 1;
                        *offset += 1;
                    }
                    if *offset == 2 {
                        self.state = State::Size;
                        continue;
                    }
                }
                State::Size | State::Trailers => {
                    let trailers = matches!(self.state, State::Trailers);
                    while at < input.len() {
                        self.line.push(input[at]);
                        at += 1;
                        if trailers {
                            self.trailer_bytes += 1;
                        }
                        if self.line.len() > if trailers { 64 << 10 } else { 4096 }
                            || self.trailer_bytes > 64 << 10
                        {
                            return Err(Error("HTTP line budget exceeded"));
                        }
                        if self.line.last() == Some(&b'\n') {
                            break;
                        }
                    }
                    if self.line.last() == Some(&b'\n') {
                        if !self.line.ends_with(b"\r\n") {
                            return Err(Error("invalid HTTP framing line terminator"));
                        }
                        if trailers {
                            if self.line == b"\r\n" {
                                self.state = State::Done;
                            } else if !valid_header_line(&self.line[..self.line.len() - 2]) {
                                return Err(Error("invalid HTTP trailer"));
                            }
                        } else {
                            let line = core::str::from_utf8(&self.line[..self.line.len() - 2])
                                .map_err(|_| Error("invalid HTTP chunk header"))?;
                            let size = line.split(';').next().unwrap_or("").trim();
                            if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_hexdigit())
                            {
                                return Err(Error("invalid HTTP chunk length"));
                            }
                            let length = u64::from_str_radix(size, 16)
                                .map_err(|_| Error("HTTP chunk length overflow"))?;
                            if length > self.limit.saturating_sub(self.received) {
                                return Err(Error("HTTP body exceeds configured limit"));
                            }
                            self.state = if length == 0 {
                                State::Trailers
                            } else {
                                State::Data(length)
                            };
                        }
                        self.line.clear();
                        continue;
                    }
                }
            }
            if eof {
                return Err(Error("truncated HTTP body framing"));
            }
            return Ok(Step {
                consumed: at,
                chunk: None,
                done: false,
            });
        }
    }
}
fn valid_header_line(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|&b| b == b':') else {
        return false;
    };
    colon != 0
        && line[..colon].iter().all(|&b| token(b))
        && line[colon + 1..]
            .iter()
            .all(|&b| b == b'\t' || b >= 32 && b != 127)
}
fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

pub struct ResponseHead {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
}
/// Encode the shared HTTP/1 request head. Transport-owned framing cannot be
/// replaced by caller-supplied headers.
pub fn request_head(
    method: &str,
    url: &crate::url::Url,
    headers: &[(String, String)],
    length: Option<usize>,
    user_agent: &str,
) -> Result<String, Error> {
    use alloc::format;
    if method.is_empty() || !method.bytes().all(token) {
        return Err(Error("invalid HTTP method"));
    }
    let host = url.port.map_or_else(
        || url.hostname().into(),
        |port| format!("{}:{port}", url.hostname()),
    );
    let mut result = format!(
        "{method} {} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n",
        url.request_target()
    );
    let mut have_agent = false;
    for (name, value) in headers {
        if name.is_empty()
            || !name.bytes().all(token)
            || value
                .bytes()
                .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
        {
            return Err(Error("invalid HTTP request header"));
        }
        if ["host", "connection", "content-length", "transfer-encoding"]
            .iter()
            .any(|key| name.eq_ignore_ascii_case(key))
        {
            continue;
        }
        have_agent |= name.eq_ignore_ascii_case("user-agent");
        result.push_str(&format!("{name}: {value}\r\n"));
    }
    if !have_agent {
        result.push_str(&format!("User-Agent: {user_agent}\r\n"));
    }
    if let Some(length) = length {
        result.push_str(&format!("Content-Length: {length}\r\n"));
    }
    result.push_str("\r\n");
    Ok(result)
}
/// Parse one complete response head, excluding any body bytes.
pub fn response_head(bytes: &[u8]) -> Result<ResponseHead, Error> {
    if bytes.len() > 64 << 10 || !bytes.ends_with(b"\r\n\r\n") {
        return Err(Error("invalid HTTP response head"));
    }
    let mut lines = bytes.split(|&byte| byte == b'\n');
    let status = lines.next().ok_or(Error("missing HTTP status"))?;
    let status = core::str::from_utf8(
        status
            .strip_suffix(b"\r")
            .ok_or(Error("invalid HTTP status terminator"))?,
    )
    .map_err(|_| Error("invalid HTTP status"))?;
    let mut parts = status.splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(Error("unsupported HTTP version"));
    }
    let code = parts.next().ok_or(Error("missing HTTP status code"))?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error("invalid HTTP status code"));
    }
    let status = code
        .parse::<u16>()
        .map_err(|_| Error("invalid HTTP status code"))?;
    if !(100..600).contains(&status) || status == 101 {
        return Err(Error("unsupported HTTP status"));
    }
    let status_text = parts.next().unwrap_or("");
    if status_text.bytes().any(|byte| byte < 32 || byte == 127) {
        return Err(Error("invalid HTTP status text"));
    }
    let mut headers = Vec::new();
    for line in lines {
        if line == b"\r" || line.is_empty() {
            break;
        }
        let line = line
            .strip_suffix(b"\r")
            .ok_or(Error("invalid HTTP header terminator"))?;
        if !valid_header_line(line) {
            return Err(Error("invalid HTTP response header"));
        }
        let colon = line.iter().position(|&b| b == b':').unwrap();
        let name = core::str::from_utf8(&line[..colon])
            .unwrap()
            .to_ascii_lowercase();
        let value = line[colon + 1..]
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>()
            .trim()
            .into();
        headers.push((name, value));
    }
    Ok(ResponseHead {
        status,
        status_text: status_text.into(),
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronous_http_messages_roundtrip_binary_bodies_and_validate_lengths() {
        let request = SyncHttpRequest {
            method: "POST".into(),
            url: "https://example.test/path".into(),
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            body: Some(vec![0, 0xff, b'a']),
            mode: "cors".into(),
            credentials: "include".into(),
            redirect: "follow".into(),
            force_preflight: true,
            timeout_ms: 2000,
            origin: Some("https://caller.test".into()),
        };
        assert_eq!(
            SyncHttpRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );

        let response = SyncHttpResponse {
            status: 201,
            status_text: "Created".into(),
            url: "https://example.test/path".into(),
            headers: vec![("x-result".into(), "ok".into())],
            body: vec![0, 0xfe, b'z'],
        };
        assert_eq!(
            SyncHttpResponse::decode(&response.encode().unwrap()).unwrap(),
            response
        );
        assert!(SyncHttpRequest::decode(b"XHR1\xff").is_err());
    }

    #[test]
    fn shared_http_heads_validate_wire_boundaries_and_owned_framing_headers() {
        let url = crate::url::parse("http://example.test:8080/a?q=1", None).unwrap();
        let head = request_head(
            "POST",
            &url,
            &[
                ("Host".into(), "other.test".into()),
                ("Content-Length".into(), "999".into()),
            ],
            Some(3),
            "Lumen",
        )
        .unwrap();
        assert!(head.starts_with("POST /a?q=1 HTTP/1.1\r\nHost: example.test:8080\r\n"));
        assert!(head.contains("Content-Length: 3\r\n"));
        assert!(!head.contains("999") && !head.contains("other.test"));
        assert!(request_head("GET\r\n", &url, &[], None, "Lumen").is_err());
        assert!(
            request_head(
                "GET",
                &url,
                &[("X-Test".into(), "ok\r\ninjected: yes".into())],
                None,
                "Lumen"
            )
            .is_err()
        );
        assert_eq!(
            response_head(b"HTTP/1.1 100 Continue\r\n\r\n")
                .unwrap()
                .status,
            100
        );
        let response =
            response_head(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nX-Byte: \xe9\r\n\r\n").unwrap();
        assert_eq!(response.headers[1], ("x-byte".into(), "é".into()));
        for head in [
            b"HTTP/1.1 200 OK\n\n".as_slice(),
            b"HTTP/1.1 101 Switching Protocols\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n Folded: x\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nBad Header: x\r\n\r\n",
        ] {
            assert!(response_head(head).is_err());
        }
    }
    #[test]
    fn asynchronous_chunk_framing_retains_every_split_and_delivers_before_eof() {
        let encoded = b"3;extension=yes\r\nabc\r\n2\r\nde\r\n0\r\nX-Result: ok\r\n\r\n";
        for width in 1..encoded.len() {
            let mut decoder = Decoder::new(Framing::Chunked, 5).unwrap();
            let mut body = Vec::new();
            for window in encoded.chunks(width) {
                let mut at = 0;
                while at < window.len() {
                    let step = decoder.decode(&window[at..], false, 2).unwrap();
                    assert!(step.consumed > 0);
                    at += step.consumed;
                    if let Some(chunk) = step.chunk {
                        body.extend(chunk);
                    }
                }
            }
            assert_eq!(body, b"abcde");
            assert!(decoder.is_done());
        }
    }
    #[test]
    fn incomplete_malformed_and_excess_bodies_fail_without_reusing_decoder() {
        for data in [
            b"2\r\na".as_slice(),
            b"1\r\na!\n",
            b"0\r\ninvalid\r\n\r\n",
            b"3\r\nabc\r\n0\r\n\r\n",
        ] {
            let mut decoder = Decoder::new(Framing::Chunked, 2).unwrap();
            let mut at = 0;
            loop {
                match decoder.decode(&data[at..], true, 64) {
                    Err(_) => break,
                    Ok(step) => {
                        at += step.consumed;
                        assert!(!step.done);
                    }
                }
            }
            assert!(decoder.decode(&[], true, 64).is_err());
        }
        let mut decoder = Decoder::new(Framing::Eof, 2).unwrap();
        assert_eq!(
            decoder.decode(b"ab", false, 64).unwrap().chunk.unwrap(),
            b"ab"
        );
        assert!(decoder.decode(b"c", true, 64).is_err());
        assert!(Decoder::new(Framing::Length(3), 2).is_err());
    }
    #[test]
    fn response_framing_rejects_conflicts_and_preserves_no_body_rules() {
        let headers = vec![("Content-Length".into(), "3, 4".into())];
        assert!(framing("GET", 200, &headers).is_err());
        assert_eq!(framing("HEAD", 200, &headers).unwrap(), Framing::Empty);
        let headers = vec![("Transfer-Encoding".into(), "gzip, chunked".into())];
        assert!(framing("GET", 200, &headers).is_err());
        let mut decoder = Decoder::new(Framing::Length(3), 3).unwrap();
        let step = decoder.decode(b"abcnext response", false, 32).unwrap();
        assert_eq!(step.consumed, 3);
        assert!(step.done);
    }
}
