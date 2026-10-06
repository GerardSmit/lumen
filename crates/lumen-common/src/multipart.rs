//! `multipart/form-data` bodies: the WHATWG HTML encoding of an entry list and a parser that also
//! reads bodies other clients produce (RFC 7578). Language-neutral: the parser returns byte ranges
//! of its input, so a caller can share the bytes of a file part instead of copying them.

use crate::search;
use std::ops::Range;

/// The value of one entry to encode.
pub enum Value<'a> {
    Text(&'a str),
    File {
        filename: &'a str,
        content_type: &'a str,
        bytes: &'a [u8],
    },
}

/// One entry of a `multipart/form-data` body.
pub struct Entry<'a> {
    pub name: &'a str,
    pub value: Value<'a>,
}

/// One part of a parsed body: a file when `filename` is present.
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: String,
    /// The part's content within the parsed input.
    pub body: Range<usize>,
}

/// Every CR and LF not part of a CRLF pair becomes CRLF.
fn normalize_newlines(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                if bytes.get(at + 1) == Some(&b'\n') {
                    at += 1;
                }
            }
            b'\n' => out.extend_from_slice(b"\r\n"),
            byte => out.push(byte),
        }
        at += 1;
    }
}

/// A field name or filename as it appears inside a quoted `Content-Disposition` parameter.
fn escape_parameter(text: &str, out: &mut Vec<u8>) {
    let mut normalized = Vec::with_capacity(text.len());
    normalize_newlines(text, &mut normalized);
    for byte in normalized {
        match byte {
            b'\n' => out.extend_from_slice(b"%0A"),
            b'\r' => out.extend_from_slice(b"%0D"),
            b'"' => out.extend_from_slice(b"%22"),
            byte => out.push(byte),
        }
    }
}

/// Serializes `entries` with `boundary` (without the leading dashes of the delimiter).
pub fn encode<'a>(entries: impl IntoIterator<Item = Entry<'a>>, boundary: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        escape_parameter(entry.name, &mut out);
        out.push(b'"');
        match entry.value {
            Value::Text(text) => {
                out.extend_from_slice(b"\r\n\r\n");
                normalize_newlines(text, &mut out);
            }
            Value::File {
                filename,
                content_type,
                bytes,
            } => {
                out.extend_from_slice(b"; filename=\"");
                escape_parameter(filename, &mut out);
                out.extend_from_slice(b"\"\r\nContent-Type: ");
                out.extend_from_slice(content_type.as_bytes());
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

fn unescape_parameter(text: &str) -> String {
    text.replace("%22", "\"").replace("%0A", "\n").replace("%0D", "\r")
}

/// The `key=value` parameters of a header value, with quoted strings unquoted. Segments split
/// at semicolons outside quotes; a segment without `=` (the disposition type) is skipped.
fn header_parameters(value: &str) -> Vec<(String, String)> {
    let mut segments = vec![String::new()];
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        let segment = segments.last_mut().expect("one segment exists");
        match character {
            '"' => {
                quoted = !quoted;
                segment.push(character);
            }
            '\\' if quoted => match chars.next() {
                Some('"') => segment.push_str("\\\""),
                Some(escaped) => {
                    segment.push('\\');
                    segment.push(escaped);
                }
                None => segment.push('\\'),
            },
            ';' if !quoted => segments.push(String::new()),
            other => segment.push(other),
        }
    }
    segments
        .iter()
        .filter_map(|segment| {
            let (key, raw) = segment.split_once('=')?;
            let raw = raw.trim();
            let text = match raw.strip_prefix('"') {
                Some(inner) => inner.strip_suffix('"').unwrap_or(inner).replace("\\\"", "\""),
                None => raw.to_owned(),
            };
            Some((key.trim().to_ascii_lowercase(), text))
        })
        .collect()
}

/// Parses `bytes` delimited by `boundary`. Parts without a `name` are skipped.
pub fn decode(bytes: &[u8], boundary: &str) -> Vec<Part> {
    let delimiter = [b"--".as_slice(), boundary.as_bytes()].concat();
    let next_delimiter = [b"\r\n".as_slice(), &delimiter].concat();
    let mut parts = Vec::new();
    let mut at = match search::find(bytes, &delimiter) {
        Some(at) => at + delimiter.len(),
        None => return parts,
    };
    loop {
        if bytes[at..].starts_with(b"--") {
            break;
        }
        while matches!(bytes.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        if bytes[at..].starts_with(b"\r\n") {
            at += 2;
        }
        let (headers, body_start) = if bytes[at..].starts_with(b"\r\n") {
            (&bytes[at..at], at + 2)
        } else {
            let Some(end) = search::find_from(bytes, b"\r\n\r\n", at) else {
                break;
            };
            (&bytes[at..end], end + 4)
        };
        let Some(body_end) = search::find_from(bytes, &next_delimiter, body_start) else {
            break;
        };
        let mut name = None;
        let mut filename = None;
        let mut content_type = String::new();
        for line in String::from_utf8_lossy(headers).split("\r\n") {
            let Some((header, value)) = line.split_once(':') else {
                continue;
            };
            match header.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    for (key, parameter) in header_parameters(value) {
                        match key.as_str() {
                            "name" => name = Some(unescape_parameter(&parameter)),
                            "filename" => filename = Some(unescape_parameter(&parameter)),
                            _ => {}
                        }
                    }
                }
                "content-type" => content_type = value.trim().to_owned(),
                _ => {}
            }
        }
        if let Some(name) = name {
            parts.push(Part {
                name,
                filename,
                content_type,
                body: body_start..body_end,
            });
        }
        at = body_end + next_delimiter.len();
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_text_and_file_entries() {
        let body = encode(
            [
                Entry {
                    name: "a\"b",
                    value: Value::Text("x\ny"),
                },
                Entry {
                    name: "f",
                    value: Value::File {
                        filename: "n\".txt",
                        content_type: "text/plain",
                        bytes: b"\r\n--B data",
                    },
                },
            ],
            "B",
        );
        let parts = decode(&body, "B");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "a\"b");
        assert_eq!(&body[parts[0].body.clone()], b"x\r\ny");
        assert_eq!(parts[0].filename, None);
        assert_eq!(parts[1].filename.as_deref(), Some("n\".txt"));
        assert_eq!(parts[1].content_type, "text/plain");
    }

    #[test]
    fn reads_foreign_bodies_with_preamble_and_empty_parts() {
        let body = b"preamble\r\n--b\r\nContent-Disposition: form-data; filename=\"q\"; name=\"k\"\r\n\r\n\r\n--b--\r\n";
        let parts = decode(body, "b");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "k");
        assert_eq!(parts[0].filename.as_deref(), Some("q"));
        assert!(parts[0].body.is_empty());
    }
}
