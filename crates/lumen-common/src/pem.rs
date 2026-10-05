//! PEM framing as OpenSSL reads and writes it: text outside blocks is skipped, RFC 1421 headers
//! (`Proc-Type`, `DEK-Info`) are kept, whitespace inside the base64 body is ignored and lines are
//! 64 columns wide. The `pem-rfc7468` crate is not used because it is strict (no headers, no
//! surrounding text), which OpenSSL-compatible input needs.

use crate::codec::{self, Padding};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PemError {
    /// The block has no matching `-----END` line (or no block has the wanted label).
    NoStartLine,
    BadBase64,
}

/// One `-----BEGIN label-----` block: its RFC 1421 headers and decoded body (empty when `error`
/// says why the block is malformed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub label: String,
    pub headers: Vec<(String, String)>,
    pub data: Vec<u8>,
    pub error: Option<PemError>,
}

const BEGIN: &str = "-----BEGIN ";

/// Every block in `input`, in order.
pub fn blocks(input: &[u8]) -> Vec<PemBlock> {
    let text = String::from_utf8_lossy(input);
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start + BEGIN.len()..];
        let Some(label_end) = after.find("-----") else { break };
        let label = after[..label_end].to_string();
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(end) = body_start.find(&end_marker) else {
            out.push(PemBlock { label, headers: Vec::new(), data: Vec::new(), error: Some(PemError::NoStartLine) });
            rest = body_start;
            continue;
        };
        let body = &body_start[..end];
        rest = &body_start[end + end_marker.len()..];
        let mut headers = Vec::new();
        let mut b64 = String::new();
        for line in body.lines() {
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            } else {
                b64.extend(line.chars().filter(|c| !c.is_whitespace()));
            }
        }
        let (data, error) = match codec::base64_decode_strict(b64.as_bytes(), false, Padding::Required) {
            Ok(data) => (data, None),
            Err(_) => (Vec::new(), Some(PemError::BadBase64)),
        };
        out.push(PemBlock { label, headers, data, error });
    }
    out
}

/// The well-formed blocks only.
pub fn well_formed(input: &[u8]) -> Vec<PemBlock> {
    let mut all = blocks(input);
    all.retain(|b| b.error.is_none());
    all
}

/// The body of the first block whose label is one of `labels`.
pub fn find(input: &[u8], labels: &[&str]) -> Result<Vec<u8>, PemError> {
    blocks(input)
        .into_iter()
        .find(|b| labels.contains(&b.label.as_str()))
        .map_or(Err(PemError::NoStartLine), |b| b.error.map_or(Ok(b.data), Err))
}

/// PEM text with 64-column base64 lines, with `headers` (and the blank line after them) when any.
pub fn encode(label: &str, headers: &[(&str, String)], data: &[u8]) -> String {
    let b64 = codec::base64_encode(data, false, true);
    let mut out = format!("-----BEGIN {label}-----\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    if !headers.is_empty() {
        out.push('\n');
    }
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_headers() {
        let data: Vec<u8> = (0..100).collect();
        let text = encode("TEST", &[("Proc-Type", "4,ENCRYPTED".to_string())], &data);
        assert!(text.lines().all(|l| l.len() <= 64));
        let blocks = blocks(format!("junk\n{text}more junk").as_bytes());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].label, "TEST");
        assert_eq!(blocks[0].headers, vec![("Proc-Type".to_string(), "4,ENCRYPTED".to_string())]);
        assert_eq!(blocks[0].data, data);
    }

    #[test]
    fn find_reports_errors() {
        let ok = encode("CERTIFICATE", &[], b"abc");
        assert_eq!(find(ok.as_bytes(), &["CERTIFICATE"]), Ok(b"abc".to_vec()));
        assert_eq!(find(ok.as_bytes(), &["OTHER"]), Err(PemError::NoStartLine));
        assert_eq!(find(b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----", &["CERTIFICATE"]), Err(PemError::BadBase64));
        assert_eq!(find(b"-----BEGIN CERTIFICATE-----\nAAAA\n", &["CERTIFICATE"]), Err(PemError::NoStartLine));
    }

    #[test]
    fn skips_other_labels() {
        let text = format!("{}{}", encode("A", &[], b"1"), encode("B", &[], b"2"));
        assert_eq!(find(text.as_bytes(), &["B"]), Ok(b"2".to_vec()));
    }
}
