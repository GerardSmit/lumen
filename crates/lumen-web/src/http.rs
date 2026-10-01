//! A blocking HTTP/1.1 client on `std::net::TcpStream`, run on the threadpool by the fetch
//! op. `Connection: close` per request (no pooling), Content-Length and chunked bodies,
//! redirects followed up to 5 hops. HTTPS goes through lumen-tls (the dynamically loaded system
//! OpenSSL on Unix, rustls on Windows) with CA and hostname verification.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::url;

pub(crate) struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Where the response actually came from (after redirects).
    pub url: String,
}

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Response-body cap so a hostile server can't balloon the worker (32 MiB).
const MAX_BODY: u64 = 32 << 20;
/// Cap on the status/request line plus headers (and on chunked trailers).
pub(crate) const MAX_HEADER_BYTES: usize = 64 << 10;
const MAX_CHUNK_SIZE_LINE: usize = 4096;
const INITIAL_BODY_CAPACITY: u64 = 64 << 10;

/// Read one line without ever buffering more than `*budget` bytes; errors once the line would
/// exceed what is left of the budget. Returns an empty string at EOF.
pub(crate) fn read_capped_line(
    reader: &mut impl BufRead,
    budget: &mut usize,
) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n > *budget {
        return Err(std::io::Error::other("headers too large"));
    }
    *budget -= n;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read `len` bytes (capped at `MAX_BODY`), growing the buffer as data arrives rather than
/// trusting the declared length for the allocation.
pub(crate) fn read_body_exact(reader: &mut impl BufRead, len: u64) -> std::io::Result<Vec<u8>> {
    let want = len.min(MAX_BODY);
    let mut body = Vec::with_capacity(want.min(INITIAL_BODY_CAPACITY) as usize);
    reader.by_ref().take(want).read_to_end(&mut body)?;
    if (body.len() as u64) < want {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(body)
}

pub(crate) fn request(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse, String> {
    let mut method = method.to_ascii_uppercase();
    let mut target = target.to_string();
    let mut body = body.map(|b| b.to_vec());
    for _ in 0..=MAX_REDIRECTS {
        let u = url::parse(&target, None)?;
        match u.scheme.as_str() {
            "http" | "https" => {}
            other => return Err(format!("fetch: unsupported scheme '{other}'")),
        }
        let response = one_request(&method, &u, headers, body.as_deref())?;
        match response.status {
            301 | 302 | 303 | 307 | 308 => {
                let Some(location) = header(&response.headers, "location") else {
                    return Ok(HttpResponse {
                        url: u.href(),
                        ..response
                    });
                };
                target = url::parse(&location, Some(&u.href()))?.href();
                // 303 (and historically 301/302) switch to GET and drop the body.
                if response.status == 303
                    || ((response.status == 301 || response.status == 302) && method == "POST")
                {
                    method = "GET".to_string();
                    body = None;
                }
            }
            _ => {
                return Ok(HttpResponse {
                    url: u.href(),
                    ..response
                })
            }
        }
    }
    Err(format!("fetch '{target}': too many redirects"))
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
) -> Result<HttpResponse, String> {
    let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
    let host_header = match u.port {
        Some(p) => format!("{}:{}", u.hostname(), p),
        None => u.hostname().to_string(),
    };
    let stream = TcpStream::connect((u.hostname().trim_matches(['[', ']']), port))
        .map_err(|e| format!("fetch '{}': connect: {e}", u.href()))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n",
        u.request_target()
    );
    let mut have_ua = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("connection") {
            continue; // we own these
        }
        have_ua |= k.eq_ignore_ascii_case("user-agent");
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !have_ua {
        req.push_str(concat!(
            "User-Agent: lumen/",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        ));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");

    let mut stream: Box<dyn ReadWrite> = if u.scheme == "https" {
        Box::new(
            lumen_tls::TlsStream::connect(stream, u.hostname().trim_matches(['[', ']']))
                .map_err(|error| format!("fetch '{}': {error}", u.href()))?,
        )
    } else {
        Box::new(stream)
    };
    stream
        .write_all(req.as_bytes())
        .and_then(|()| body.map_or(Ok(()), |b| stream.write_all(b)))
        .map_err(|e| format!("fetch '{}': write: {e}", u.href()))?;

    let mut reader = BufReader::new(stream);
    let mut header_budget = MAX_HEADER_BYTES;
    let status_line = read_capped_line(&mut reader, &mut header_budget)
        .map_err(|e| format!("fetch '{}': read: {e}", u.href()))?;
    // "HTTP/1.1 200 OK"
    let mut parts = status_line.trim_end().splitn(3, ' ');
    let _version = parts.next().unwrap_or("");
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
        format!(
            "fetch '{}': malformed status line {status_line:?}",
            u.href()
        )
    })?;
    let status_text = parts.next().unwrap_or("").to_string();

    let mut headers_out = Vec::new();
    loop {
        let line = read_capped_line(&mut reader, &mut header_budget)
            .map_err(|e| format!("fetch '{}': read headers: {e}", u.href()))?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(i) = line.find(':') {
            headers_out.push((line[..i].to_string(), line[i + 1..].trim().to_string()));
        }
    }

    let body = read_body(&mut reader, &headers_out, method, status)
        .map_err(|e| format!("fetch '{}': body: {e}", u.href()))?;
    Ok(HttpResponse {
        status,
        status_text,
        headers: headers_out,
        body,
        url: String::new(), // stamped by the redirect loop
    })
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn read_body(
    reader: &mut impl BufRead,
    headers: &[(String, String)],
    method: &str,
    status: u16,
) -> std::io::Result<Vec<u8>> {
    if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Vec::new());
    }
    if header(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader);
    }
    if let Some(len) = header(headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
        return read_body_exact(reader, len);
    }
    // No framing: Connection: close means read to EOF.
    let mut body = Vec::new();
    reader.take(MAX_BODY).read_to_end(&mut body)?;
    Ok(body)
}

pub(crate) fn read_chunked(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line_budget = MAX_CHUNK_SIZE_LINE;
        let size_line = read_capped_line(reader, &mut line_budget)?;
        let size = usize::from_str_radix(
            size_line.trim_end().split(';').next().unwrap_or("").trim(),
            16,
        )
        .map_err(|_| std::io::Error::other(format!("bad chunk size {size_line:?}")))?;
        if size == 0 {
            // Trailer section (usually just the final CRLF).
            let mut trailer_budget = MAX_HEADER_BYTES;
            loop {
                let trailer = read_capped_line(reader, &mut trailer_budget)?;
                if trailer.is_empty() || trailer.trim_end().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() as u64 + size as u64 > MAX_BODY {
            return Err(std::io::Error::other("response body too large"));
        }
        let start = body.len();
        reader.by_ref().take(size as u64).read_to_end(&mut body)?;
        if body.len() - start < size {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_decoding() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut r = std::io::BufReader::new(&raw[..]);
        assert_eq!(read_chunked(&mut r).unwrap(), b"Wikipedia");
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
    fn header_budget_is_shared_across_lines() {
        let raw = b"aaaa\r\nbbbb\r\n";
        let mut r = std::io::Cursor::new(&raw[..]);
        let mut budget = 10;
        assert_eq!(read_capped_line(&mut r, &mut budget).unwrap(), "aaaa\r\n");
        assert!(read_capped_line(&mut r, &mut budget).is_err());
    }

    #[test]
    fn oversized_chunk_size_line_is_rejected() {
        let mut raw = vec![b'1'; 100_000];
        raw.extend_from_slice(b"\r\n");
        let mut r = std::io::Cursor::new(raw);
        assert!(read_chunked(&mut r).is_err());
    }

    #[test]
    fn huge_content_length_with_small_body_does_not_preallocate() {
        struct Probe<'a>(std::io::Cursor<&'a [u8]>);
        impl Read for Probe<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        let mut reader = BufReader::new(Probe(std::io::Cursor::new(b"tiny")));
        let err = read_body_exact(&mut reader, u64::MAX).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

        let mut cursor = std::io::Cursor::new(b"hello world".to_vec());
        let body = read_body_exact(&mut cursor, 5).unwrap();
        assert_eq!(body, b"hello");
        assert!(body.capacity() <= INITIAL_BODY_CAPACITY as usize);
    }

    #[test]
    #[ignore = "requires external network and a system OpenSSL trust store"]
    fn https_uses_verified_tls() {
        let response = request("GET", "https://example.com/", &[], None).unwrap();
        assert_eq!(response.status, 200);
    }
}
