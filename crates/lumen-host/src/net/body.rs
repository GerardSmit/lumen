//! `BodyInit` extraction: the bytes of a request body and the `Content-Type` it implies.

use crate::blob::{blob_of, encode_form_data, is_form_data};
use crate::url::bindings::WebUrlSearchParams;
use lumen::embed::{Ctx, OpError, OpResult, Value};

/// A request body ready for the transport.
pub struct ExtractedBody {
    pub bytes: Vec<u8>,
    /// The type to send when the request has no `Content-Type` of its own.
    pub content_type: Option<String>,
}

/// Extract the body of `value`: a `Blob`/`File`, a `BufferSource`, a `FormData`, a
/// `URLSearchParams`, or anything else as its string form.
pub fn extract_body(ctx: &mut Ctx, value: &Value) -> OpResult<ExtractedBody> {
    if let Value::Obj(_) = value {
        if let Some(blob) = blob_of(ctx, value) {
            let bytes = blob.source.bytes(ctx)?.to_vec();
            let content_type = (!blob.content_type.is_empty()).then_some(blob.content_type);
            return Ok(ExtractedBody { bytes, content_type });
        }
        if is_form_data(ctx, value) {
            let form = encode_form_data(ctx, value)?;
            return Ok(ExtractedBody {
                bytes: form.body,
                content_type: Some(form.content_type),
            });
        }
        if let Some(bytes) = ctx.buffer_source_bytes(value) {
            return Ok(ExtractedBody {
                bytes,
                content_type: None,
            });
        }
        if ctx.instance_data::<WebUrlSearchParams>(value).is_some() {
            let text = to_text(ctx, value)?;
            return Ok(ExtractedBody {
                bytes: text.into_bytes(),
                content_type: Some("application/x-www-form-urlencoded;charset=UTF-8".into()),
            });
        }
    }
    let text = to_text(ctx, value)?;
    Ok(ExtractedBody {
        bytes: text.into_bytes(),
        content_type: Some("text/plain;charset=UTF-8".into()),
    })
}

fn to_text(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    ctx.coerce_string(value)
        .map(|text| text.to_string())
        .map_err(OpError::thrown)
}

/// The value of the `charset` parameter of a media type, unquoted.
pub(crate) fn charset_of(media_type: &str) -> Option<String> {
    let lower = media_type.to_ascii_lowercase();
    let at = lower.find("charset")?;
    let rest = lower[at + "charset".len()..].trim_start().strip_prefix('=')?;
    let rest = rest.trim_start().trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ';' | '"' | '\''))
        .unwrap_or(rest.len());
    let label = &rest[..end];
    (!label.is_empty()).then(|| label.to_owned())
}

/// The essence of a media type: type and subtype, lowercased, without parameters.
pub(crate) fn essence_of(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Decode `bytes` with the encoding `charset` names, UTF-8 when it is absent or unknown. A byte
/// order mark is consumed.
pub(crate) fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    let decoder = charset
        .and_then(|label| lumen_common::encoding::TextDecoder::new(label, false, false).ok())
        .or_else(|| lumen_common::encoding::TextDecoder::new("utf-8", false, false).ok());
    decoder
        .and_then(|mut decoder| decoder.decode(bytes, false).ok())
        .unwrap_or_default()
}
