//! Bounded WHATWG streaming byte decoding, independent of a language or OS.
use alloc::string::String;
use encoding_rs::{CoderResult, Decoder, DecoderResult, Encoder, Encoding, REPLACEMENT};

mod html;
pub use html::prescan_html_encoding;
mod css;
pub use css::stylesheet_encoding;

pub const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnsupportedLabel,
    Malformed,
    ResourceLimit,
}

pub fn canonical_label(label: &str) -> Result<&'static str, DecodeError> {
    Ok(resolve_label(label)?.name())
}

/// Document decoding accepts the replacement encoding that TextDecoder's
/// JavaScript constructor deliberately rejects.
pub fn canonical_document_label(label: &str) -> Result<&'static str, DecodeError> {
    Encoding::for_label(label.as_bytes())
        .map(Encoding::name)
        .ok_or(DecodeError::UnsupportedLabel)
}

/// WHATWG output encodings map UTF-16 and replacement to UTF-8.
pub fn canonical_output_label(label: &str) -> Result<&'static str, DecodeError> {
    Encoding::for_label(label.as_bytes())
        .map(|encoding| encoding.output_encoding().name())
        .ok_or(DecodeError::UnsupportedLabel)
}

/// A bounded streaming encoder for legacy web resource and form output.
/// Unmappable characters use decimal HTML references, as required by the
/// Encoding Standard's encode algorithm. The caller owns the output sink.
pub struct OutputEncoder {
    encoder: Encoder,
    output_limit: usize,
    written: usize,
    finished: bool,
}

impl OutputEncoder {
    pub fn new(label: &str, output_limit: usize) -> Result<Self, DecodeError> {
        let encoding = Encoding::for_label(label.as_bytes())
            .ok_or(DecodeError::UnsupportedLabel)?
            .output_encoding();
        Ok(Self {
            encoder: encoding.new_encoder(),
            output_limit,
            written: 0,
            finished: false,
        })
    }

    /// Feed borrowed UTF-8 without allocating a whole-input encoded copy.
    /// `last` flushes stateful encodings; further input is then rejected.
    pub fn write(
        &mut self,
        input: &str,
        last: bool,
        sink: &mut impl FnMut(&[u8]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        if self.finished {
            return Err(DecodeError::Malformed);
        }
        let mut consumed = 0;
        let mut scratch = [0u8; 4096];
        loop {
            let (result, read, written, _) =
                self.encoder
                    .encode_from_utf8(&input[consumed..], &mut scratch, last);
            consumed += read;
            let Some(total) = self
                .written
                .checked_add(written)
                .filter(|total| *total <= self.output_limit)
            else {
                self.finished = true;
                return Err(DecodeError::ResourceLimit);
            };
            self.written = total;
            if let Err(error) = sink(&scratch[..written]) {
                self.finished = true;
                return Err(error);
            }
            match result {
                CoderResult::InputEmpty => {
                    self.finished = last;
                    return Ok(());
                }
                CoderResult::OutputFull if read == 0 && written == 0 => {
                    self.finished = true;
                    return Err(DecodeError::ResourceLimit);
                }
                CoderResult::OutputFull => {}
            }
        }
    }
}

fn resolve_label(label: &str) -> Result<&'static Encoding, DecodeError> {
    Encoding::for_label(label.as_bytes())
        .filter(|encoding| *encoding != REPLACEMENT)
        .ok_or(DecodeError::UnsupportedLabel)
}

pub struct TextDecoder {
    encoding: &'static Encoding,
    decoder: Decoder,
    fatal: bool,
    ignore_bom: bool,
    output_limit: usize,
}

impl TextDecoder {
    pub fn new(label: &str, fatal: bool, ignore_bom: bool) -> Result<Self, DecodeError> {
        Self::new_bounded(label, fatal, ignore_bom, MAX_DECODED_BYTES)
    }

    pub fn new_bounded(
        label: &str,
        fatal: bool,
        ignore_bom: bool,
        output_limit: usize,
    ) -> Result<Self, DecodeError> {
        Ok(Self::from_encoding(
            resolve_label(label)?,
            fatal,
            ignore_bom,
            output_limit,
        ))
    }

    /// Decode document bytes after the caller has handled a BOM and selected
    /// the document encoding. Malformed input uses replacement characters.
    pub fn new_document_bounded(label: &str, output_limit: usize) -> Result<Self, DecodeError> {
        let encoding =
            Encoding::for_label(label.as_bytes()).ok_or(DecodeError::UnsupportedLabel)?;
        Ok(Self::from_encoding(encoding, false, true, output_limit))
    }

    fn from_encoding(
        encoding: &'static Encoding,
        fatal: bool,
        ignore_bom: bool,
        output_limit: usize,
    ) -> Self {
        Self {
            encoding,
            decoder: Self::fresh_decoder(encoding, ignore_bom),
            fatal,
            ignore_bom,
            output_limit: output_limit.min(MAX_DECODED_BYTES),
        }
    }

    fn fresh_decoder(encoding: &'static Encoding, ignore_bom: bool) -> Decoder {
        if ignore_bom {
            encoding.new_decoder_without_bom_handling()
        } else {
            encoding.new_decoder_with_bom_removal()
        }
    }

    pub fn encoding(&self) -> &'static str {
        self.encoding.name()
    }

    fn reset(&mut self) {
        self.decoder = Self::fresh_decoder(self.encoding, self.ignore_bom);
    }

    /// The codec retains only its small incomplete-sequence state between calls.
    /// A fixed output scratch buffer prevents worst-case whole-input allocation.
    pub fn decode(&mut self, input: &[u8], stream: bool) -> Result<String, DecodeError> {
        let result = self.decode_inner(input, !stream);
        if !stream || matches!(result, Err(DecodeError::ResourceLimit)) {
            self.reset();
        }
        result
    }

    fn decode_inner(&mut self, mut input: &[u8], last: bool) -> Result<String, DecodeError> {
        let mut output = String::new();
        let mut scratch = [0u8; 4096];
        loop {
            let (finished, read, written, malformed) = if self.fatal {
                let (status, read, written) =
                    self.decoder
                        .decode_to_utf8_without_replacement(input, &mut scratch, last);
                (
                    status == DecoderResult::InputEmpty,
                    read,
                    written,
                    matches!(status, DecoderResult::Malformed(..)),
                )
            } else {
                let (status, read, written, _) =
                    self.decoder.decode_to_utf8(input, &mut scratch, last);
                (status == CoderResult::InputEmpty, read, written, false)
            };
            if malformed {
                return Err(DecodeError::Malformed);
            }
            let length = output
                .len()
                .checked_add(written)
                .filter(|length| *length <= self.output_limit)
                .ok_or(DecodeError::ResourceLimit)?;
            if length > output.capacity() {
                let capacity = output
                    .capacity()
                    .saturating_mul(2)
                    .max(16)
                    .max(length)
                    .min(self.output_limit);
                output
                    .try_reserve_exact(capacity - output.len())
                    .map_err(|_| DecodeError::ResourceLimit)?;
            }
            output.push_str(
                core::str::from_utf8(&scratch[..written]).map_err(|_| DecodeError::Malformed)?,
            );
            input = &input[read..];
            if finished {
                return Ok(output);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_encoder_streams_legacy_characters_references_and_final_state_with_exact_budget() {
        let mut bytes = alloc::vec::Vec::new();
        let mut encoder = OutputEncoder::new("iso-2022-jp", 8).unwrap();
        encoder
            .write("あ", false, &mut |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        encoder
            .write("", true, &mut |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(bytes, [0x1b, 0x24, 0x42, 0x24, 0x22, 0x1b, 0x28, 0x42]);
        assert_eq!(
            encoder.write("a", false, &mut |_| Ok(())),
            Err(DecodeError::Malformed)
        );
        let mut encoder = OutputEncoder::new("iso-2022-jp", 7).unwrap();
        assert_eq!(
            encoder.write("あ", true, &mut |_| Ok(())),
            Err(DecodeError::ResourceLimit)
        );
        let mut bytes = alloc::vec::Vec::new();
        OutputEncoder::new("windows-1252", 10)
            .unwrap()
            .write("€😀", true, &mut |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(bytes, b"\x80&#128512;");
        assert_eq!(canonical_output_label("utf-16le"), Ok("UTF-8"));
        assert_eq!(canonical_output_label("replacement"), Ok("UTF-8"));
    }

    #[test]
    fn document_replacement_encoding_consumes_input_once_but_api_rejects_it() {
        assert_eq!(
            canonical_document_label("csiso2022kr").unwrap(),
            "replacement"
        );
        assert_eq!(
            canonical_label("csiso2022kr"),
            Err(DecodeError::UnsupportedLabel)
        );
        let mut decoder = TextDecoder::new_document_bounded("csiso2022kr", 3).unwrap();
        assert_eq!(decoder.decode(b"ABCabc123", false).unwrap(), "\u{fffd}");
        assert_eq!(decoder.decode(b"", false).unwrap(), "");
        let mut decoder = TextDecoder::new_document_bounded("replacement", 2).unwrap();
        assert_eq!(decoder.decode(b"A", false), Err(DecodeError::ResourceLimit));
    }

    #[test]
    fn decoder_enforces_caller_byte_budget_before_output_growth() {
        let source = [0x00, 0x08].repeat(128);
        let mut decoder = TextDecoder::new_bounded("utf-16le", false, true, 383).unwrap();
        assert_eq!(
            decoder.decode(&source, false),
            Err(DecodeError::ResourceLimit)
        );
        let mut decoder = TextDecoder::new_bounded("utf-16le", false, true, 384).unwrap();
        let output = decoder.decode(&source, false).unwrap();
        assert_eq!(output.len(), 384);
        assert!(output.capacity() <= 384);
        let mut decoder = TextDecoder::new_bounded("utf-8", false, true, 0).unwrap();
        assert_eq!(decoder.decode(b"", false).unwrap(), "");
        assert_eq!(decoder.decode(b"a", false), Err(DecodeError::ResourceLimit));
    }

    #[test]
    fn decoder_output_growth_avoids_large_short_string_allocations() {
        let mut decoder = TextDecoder::new("utf-8", false, false).unwrap();
        let short = decoder.decode(b"a", false).unwrap();
        assert_eq!(short, "a");
        assert!(
            short.capacity() <= 64,
            "short decode must not reserve a whole scratch buffer"
        );
        let source = "éあ".repeat(3000);
        let long = decoder.decode(source.as_bytes(), false).unwrap();
        assert_eq!(long, source);
        assert!(long.capacity() <= MAX_DECODED_BYTES);
        assert_eq!(decoder.decode(&[], false).unwrap().capacity(), 0);
    }

    #[test]
    fn legacy_multibyte_decoders_preserve_split_sequences_and_reset() {
        let mut decoder = TextDecoder::new("  Shift_JIS\n", true, false).unwrap();
        assert_eq!(decoder.encoding(), "Shift_JIS");
        assert_eq!(decoder.decode(&[0x82], true).unwrap(), "");
        assert_eq!(decoder.decode(&[0xa0, 0x82], true).unwrap(), "あ");
        assert_eq!(decoder.decode(&[0xa2], false).unwrap(), "い");
        assert_eq!(decoder.decode(&[0x82], false), Err(DecodeError::Malformed));
        assert_eq!(decoder.decode(b"plain", false).unwrap(), "plain");
        for (label, bytes, expected) in [
            ("gb18030", &[0xc4, 0xe3][..], "你"),
            ("big5", &[0xa7, 0x41][..], "你"),
            ("euc-kr", &[0xb0, 0xa1][..], "가"),
            ("windows-1251", &[0xcf, 0xf0][..], "Пр"),
        ] {
            assert_eq!(
                TextDecoder::new(label, false, false)
                    .unwrap()
                    .decode(bytes, false)
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn streaming_bom_fatal_recovery_and_stateful_escape_sequences() {
        let mut decoder = TextDecoder::new("utf-8", false, false).unwrap();
        assert_eq!(decoder.decode(&[0xef], true).unwrap(), "");
        assert_eq!(decoder.decode(&[0xbb, 0xbf, b'a'], false).unwrap(), "a");
        assert_eq!(
            decoder.decode(&[0xef, 0xbb, 0xbf, b'b'], false).unwrap(),
            "b"
        );
        assert_eq!(
            TextDecoder::new("utf-8", false, true)
                .unwrap()
                .decode(&[0xef, 0xbb, 0xbf], false)
                .unwrap(),
            "\u{feff}"
        );
        let mut decoder = TextDecoder::new("iso-2022-jp", true, false).unwrap();
        assert_eq!(decoder.decode(&[0x1b, b'$'], true).unwrap(), "");
        assert_eq!(decoder.decode(&[b'B', 0x24, 0x22], true).unwrap(), "あ");
        assert_eq!(
            decoder.decode(&[0x1b, b'(', b'B', b'!'], false).unwrap(),
            "!"
        );
        assert_eq!(
            decoder.decode(&[0x1b, b'$', b'B', 0xff], true),
            Err(DecodeError::Malformed)
        );
        assert_eq!(decoder.decode(&[0x24, 0x22], false).unwrap(), "あ");
        assert!(matches!(
            TextDecoder::new("replacement", false, false),
            Err(DecodeError::UnsupportedLabel)
        ));
    }
}
