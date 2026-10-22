//! HTTP content codings decoded as a stream: bytes go in as they arrive and decoded bytes come
//! out in bounded pieces, so a response body never has to be held whole. Stacked codings
//! (`Content-Encoding: gzip, br`) are decoded by a chain of stages, each pulling from the one
//! before it.

use super::{
    BrotliDecoder, BrotliStatus, ZStream, ZstdDecoder, OUTPUT_LIMIT, Z_BUF_ERROR, Z_NEED_DICT,
    Z_OK, Z_STREAM_END,
};

/// The most codings one response may stack.
const MAX_DEPTH: usize = 4;
/// What one stage hands to the next in a single step.
const HANDOFF: usize = 16 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentCoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl ContentCoding {
    fn parse(token: &str) -> Result<Option<Self>, String> {
        let known = [
            ("gzip", Some(Self::Gzip)),
            ("x-gzip", Some(Self::Gzip)),
            ("deflate", Some(Self::Deflate)),
            ("br", Some(Self::Brotli)),
            ("zstd", Some(Self::Zstd)),
            ("identity", None),
        ];
        known
            .iter()
            .find(|(name, _)| token.eq_ignore_ascii_case(name))
            .map(|(_, coding)| *coding)
            .ok_or_else(|| String::from("unsupported HTTP content coding"))
    }

    /// The `Accept-Encoding` value naming every coding this module decodes.
    pub const ACCEPT: &'static str = "gzip, deflate, br, zstd";
}

/// The codings named by `Content-Encoding` header values, in the order they were applied to the
/// body; `identity` is dropped. Fails on an unknown coding or an absurd stack.
pub fn content_codings<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ContentCoding>, String> {
    let mut codings = Vec::new();
    for value in values {
        for token in value
            .split(',')
            .map(|token| token.trim_matches(|c| matches!(c, ' ' | '\t')))
            .filter(|token| !token.is_empty())
        {
            if codings.len() == MAX_DEPTH {
                return Err(String::from("HTTP content-coding depth exceeded"));
            }
            if let Some(coding) = ContentCoding::parse(token)? {
                codings.push(coding);
            }
        }
    }
    Ok(codings)
}

enum Codec {
    Zlib(ZStream),
    /// A `deflate` coding: zlib-wrapped or raw, decided by the first two bytes as browsers do.
    Deflate(Option<ZStream>),
    Brotli(Box<BrotliDecoder>),
    Zstd(Box<ZstdDecoder>),
}

enum Run {
    Data(usize),
    NeedInput,
    End,
}

struct Stage {
    codec: Codec,
    input: Vec<u8>,
    position: usize,
    input_ended: bool,
    ended: bool,
    /// The stream completed with the output of the step that reported it.
    finishing: bool,
    produced: u64,
    scratch: Vec<u8>,
}

fn zlib_header(input: &[u8]) -> bool {
    input[0] & 0x0f == 8 && (u16::from(input[0]) << 8 | u16::from(input[1])) % 31 == 0
}

/// What a codec did in one step: bytes taken from the input, bytes written, stream complete.
type Step = (usize, usize, bool);

fn inflate_step(z: &mut ZStream, input: &[u8], output: &mut [u8], what: &str) -> Result<Step, String> {
    let step = z.run(0, input, output);
    match step.code {
        Z_STREAM_END => Ok((step.consumed, step.produced, true)),
        Z_OK | Z_BUF_ERROR => Ok((step.consumed, step.produced, false)),
        Z_NEED_DICT => Err(format!("{what}: a preset dictionary is required")),
        _ => {
            let reason = z.message().unwrap_or_else(|| String::from("invalid data"));
            Err(format!("{what}: {reason}"))
        }
    }
}

impl Stage {
    fn new(coding: ContentCoding) -> Result<Self, String> {
        let codec = match coding {
            ContentCoding::Gzip => Codec::Zlib(
                ZStream::inflate(15 + 32).map_err(|_| String::from("gzip: init failed"))?,
            ),
            ContentCoding::Deflate => Codec::Deflate(None),
            ContentCoding::Brotli => Codec::Brotli(Box::new(BrotliDecoder::new())),
            ContentCoding::Zstd => Codec::Zstd(Box::new(ZstdDecoder::new()?)),
        };
        Ok(Stage {
            codec,
            input: Vec::new(),
            position: 0,
            input_ended: false,
            ended: false,
            finishing: false,
            produced: 0,
            scratch: Vec::new(),
        })
    }

    fn push(&mut self, data: &[u8]) {
        if self.ended || data.is_empty() {
            return;
        }
        if self.position == self.input.len() {
            self.input.clear();
            self.position = 0;
        } else if self.position > 0 {
            self.input.drain(..self.position);
            self.position = 0;
        }
        self.input.extend_from_slice(data);
    }

    /// One decode step into `output`. A stream that ended leaves any bytes after it unread.
    fn run(&mut self, output: &mut [u8]) -> Result<Run, String> {
        if self.finishing {
            return Ok(Run::End);
        }
        let input = &self.input[self.position..];
        let (consumed, produced, ended) = match &mut self.codec {
            Codec::Zlib(z) => inflate_step(z, input, output, "gzip")?,
            Codec::Deflate(slot) => {
                if slot.is_none() {
                    if input.len() < 2 && !self.input_ended {
                        return Ok(Run::NeedInput);
                    }
                    let bits = if input.len() >= 2 && zlib_header(input) { 15 } else { -15 };
                    *slot = Some(
                        ZStream::inflate(bits)
                            .map_err(|_| String::from("deflate: init failed"))?,
                    );
                }
                let z = slot.as_mut().expect("deflate stream was just created");
                inflate_step(z, input, output, "deflate")?
            }
            Codec::Brotli(decoder) => {
                let (status, used, produced) = decoder.run(input, output);
                if let BrotliStatus::Error(code) = status {
                    return Err(format!("brotli: decoder error {code}"));
                }
                (used, produced, status == BrotliStatus::Success)
            }
            Codec::Zstd(decoder) => decoder.run(input, output, self.input_ended)?,
        };
        self.position += consumed;
        Ok(if produced > 0 {
            self.finishing = ended;
            Run::Data(produced)
        } else if ended {
            Run::End
        } else if consumed > 0 {
            Run::Data(0)
        } else {
            Run::NeedInput
        })
    }
}

/// What [`ContentDecoder::pull`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    Data(Vec<u8>),
    /// Every byte pushed so far is decoded; push more, or call [`ContentDecoder::finish`].
    NeedInput,
    /// The last coding's stream is complete.
    End,
}

/// A chain of streaming decoders. The decoded size of every stage is bounded by `limit`, so a
/// compression bomb fails with [`OUTPUT_LIMIT`] before it is expanded.
pub struct ContentDecoder {
    stages: Vec<Stage>,
    limit: u64,
}

impl ContentDecoder {
    /// `codings` are in the order `content_codings` returns them (the order they were applied).
    pub fn new(codings: &[ContentCoding], limit: usize) -> Result<Self, String> {
        let stages = codings
            .iter()
            .rev()
            .map(|coding| Stage::new(*coding))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ContentDecoder {
            stages,
            limit: limit as u64,
        })
    }

    pub fn is_identity(&self) -> bool {
        self.stages.is_empty()
    }

    /// Appends encoded bytes.
    pub fn push(&mut self, data: &[u8]) {
        if let Some(first) = self.stages.first_mut() {
            first.push(data);
        }
    }

    /// No more encoded bytes will follow; a stream that is still incomplete is then an error.
    pub fn finish(&mut self) {
        if let Some(first) = self.stages.first_mut() {
            first.input_ended = true;
        }
    }

    /// Decodes up to `max` bytes.
    pub fn pull(&mut self, max: usize) -> Result<Decoded, String> {
        let Some(last) = self.stages.len().checked_sub(1) else {
            return Ok(Decoded::End);
        };
        let mut out = vec![0u8; max.max(1)];
        Ok(match self.produce(last, &mut out)? {
            Run::Data(count) => {
                out.truncate(count);
                Decoded::Data(out)
            }
            Run::NeedInput => Decoded::NeedInput,
            Run::End => Decoded::End,
        })
    }

    fn produce(&mut self, index: usize, output: &mut [u8]) -> Result<Run, String> {
        loop {
            if self.stages[index].ended {
                return Ok(Run::End);
            }
            match self.stages[index].run(output)? {
                Run::Data(count) => {
                    let stage = &mut self.stages[index];
                    stage.produced += count as u64;
                    if stage.produced > self.limit {
                        return Err(OUTPUT_LIMIT.into());
                    }
                    if count > 0 {
                        return Ok(Run::Data(count));
                    }
                }
                Run::End => {
                    self.stages[index].ended = true;
                    return Ok(Run::End);
                }
                Run::NeedInput => {
                    if self.stages[index].input_ended {
                        return Err(String::from("unexpected end of compressed data"));
                    }
                    if index == 0 {
                        return Ok(Run::NeedInput);
                    }
                    let mut scratch = std::mem::take(&mut self.stages[index].scratch);
                    scratch.resize(HANDOFF, 0);
                    let upstream = self.produce(index - 1, &mut scratch);
                    let stage = &mut self.stages[index];
                    let upstream = match upstream {
                        Ok(upstream) => upstream,
                        Err(error) => {
                            stage.scratch = scratch;
                            return Err(error);
                        }
                    };
                    match upstream {
                        Run::Data(count) => stage.push(&scratch[..count]),
                        Run::NeedInput => {
                            stage.scratch = scratch;
                            return Ok(Run::NeedInput);
                        }
                        Run::End => stage.input_ended = true,
                    }
                    stage.scratch = scratch;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::{
        brotli_compress, deflate, gzip_compress, zlib_compress, zstd_compress,
    };

    fn sample() -> Vec<u8> {
        (0..300_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8 ^ (i / 997) as u8)
            .collect()
    }

    fn decode(
        codings: &[ContentCoding],
        packed: &[u8],
        piece: usize,
        limit: usize,
    ) -> Result<Vec<u8>, String> {
        let mut decoder = ContentDecoder::new(codings, limit)?;
        let mut out = Vec::new();
        let mut chunks = packed.chunks(piece);
        loop {
            match decoder.pull(4096)? {
                Decoded::Data(data) => out.extend_from_slice(&data),
                Decoded::End => return Ok(out),
                Decoded::NeedInput => match chunks.next() {
                    Some(chunk) => decoder.push(chunk),
                    None => decoder.finish(),
                },
            }
        }
    }

    fn encode(coding: ContentCoding, data: &[u8]) -> Vec<u8> {
        match coding {
            ContentCoding::Gzip => gzip_compress(data),
            ContentCoding::Deflate => zlib_compress(data),
            ContentCoding::Brotli => brotli_compress(data, 5, 22),
            ContentCoding::Zstd => zstd_compress(data),
        }
    }

    const ALL: [ContentCoding; 4] = [
        ContentCoding::Gzip,
        ContentCoding::Deflate,
        ContentCoding::Brotli,
        ContentCoding::Zstd,
    ];

    #[test]
    fn every_coding_round_trips_in_any_chunking() {
        let data = sample();
        for coding in ALL {
            let packed = encode(coding, &data);
            for piece in [1usize, 7, 1000, packed.len()] {
                if piece == 1 && packed.len() > 20_000 {
                    continue;
                }
                assert_eq!(decode(&[coding], &packed, piece, usize::MAX).unwrap(), data, "{coding:?} {piece}");
            }
        }
    }

    #[test]
    fn tiny_streams_decode_one_byte_at_a_time() {
        let data = b"hello hello hello hello".to_vec();
        for coding in ALL {
            let packed = encode(coding, &data);
            assert_eq!(decode(&[coding], &packed, 1, usize::MAX).unwrap(), data, "{coding:?}");
        }
    }

    #[test]
    fn deflate_accepts_raw_streams() {
        let data = sample();
        let packed = deflate(&data);
        assert_eq!(decode(&[ContentCoding::Deflate], &packed, 513, usize::MAX).unwrap(), data);
        assert_eq!(decode(&[ContentCoding::Deflate], &packed, 1, usize::MAX).map(|d| d.len()).unwrap(), data.len());
    }

    #[test]
    fn stacked_codings_decode_in_reverse_order() {
        let data = sample();
        let stacked = encode(ContentCoding::Brotli, &encode(ContentCoding::Gzip, &data));
        let codings = [ContentCoding::Gzip, ContentCoding::Brotli];
        for piece in [1usize, 333, stacked.len()] {
            if piece == 1 && stacked.len() > 20_000 {
                continue;
            }
            assert_eq!(decode(&codings, &stacked, piece, usize::MAX).unwrap(), data);
        }
        let triple = encode(
            ContentCoding::Zstd,
            &encode(ContentCoding::Deflate, &encode(ContentCoding::Gzip, &data)),
        );
        let codings = [ContentCoding::Gzip, ContentCoding::Deflate, ContentCoding::Zstd];
        assert_eq!(decode(&codings, &triple, 4001, usize::MAX).unwrap(), data);
    }

    #[test]
    fn corrupt_and_truncated_input_fail() {
        let data = sample();
        for coding in ALL {
            let packed = encode(coding, &data);
            let truncated = &packed[..packed.len() / 2];
            assert!(decode(&[coding], truncated, 1000, usize::MAX).is_err(), "{coding:?} truncated");
            let mut corrupt = packed.clone();
            for byte in corrupt.iter_mut().skip(10).take(64) {
                *byte = !*byte;
            }
            assert!(decode(&[coding], &corrupt, 1000, usize::MAX).is_err(), "{coding:?} corrupt");
        }
        assert!(decode(&[ContentCoding::Gzip], b"not gzip at all", 4, usize::MAX).is_err());
    }

    #[test]
    fn decoded_size_is_limited() {
        let data = vec![7u8; 1 << 20];
        for coding in ALL {
            let packed = encode(coding, &data);
            let error = decode(&[coding], &packed, 100, 100_000).unwrap_err();
            assert_eq!(error, OUTPUT_LIMIT, "{coding:?}");
            assert_eq!(decode(&[coding], &packed, 100, data.len()).unwrap(), data, "{coding:?}");
        }
    }

    #[test]
    fn a_pull_never_returns_more_than_asked() {
        let data = vec![9u8; 1 << 20];
        let packed = encode(ContentCoding::Gzip, &data);
        let mut decoder = ContentDecoder::new(&[ContentCoding::Gzip], usize::MAX).unwrap();
        decoder.push(&packed);
        match decoder.pull(1000).unwrap() {
            Decoded::Data(chunk) => assert!(chunk.len() <= 1000 && !chunk.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn codings_parse_from_header_values() {
        let parse = |values: &[&str]| content_codings(values.iter().copied());
        assert_eq!(
            parse(&["GZIP, br", " identity ,zstd"]).unwrap(),
            [ContentCoding::Gzip, ContentCoding::Brotli, ContentCoding::Zstd]
        );
        assert_eq!(parse(&["x-gzip", "deflate"]).unwrap(), [ContentCoding::Gzip, ContentCoding::Deflate]);
        assert!(parse(&["identity"]).unwrap().is_empty());
        assert!(parse(&["gzip, compress"]).is_err());
        assert!(parse(&[&"gzip,".repeat(200)]).is_err());
        assert_eq!(parse(&[&"gzip,".repeat(MAX_DEPTH)]).unwrap().len(), MAX_DEPTH);
        assert!(parse(&[&"gzip,".repeat(MAX_DEPTH + 1)]).is_err());
        assert!(parse(&["gzip, gzip", "gzip, gzip", "gzip"]).is_err());
    }
}
