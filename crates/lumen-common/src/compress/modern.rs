//! Zstandard and Brotli over the maintained `zstd` / `ruzstd` and `brotli` crates, re-exported from
//! [`super`].

use brotli::enc::encode::{
    BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct,
};
use brotli::enc::StandardAlloc;
use brotli::{BrotliResult, BrotliState, HeapAlloc, HuffmanCode};

use super::{step_window, OUTPUT_LIMIT};

#[cfg(not(any(target_arch = "wasm32", target_os = "none")))]
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 0).expect("in-memory zstd compression")
}
/// Read `reader` to its end, stopping with [`OUTPUT_LIMIT`] one byte past `limit`.
fn read_limited(reader: &mut impl std::io::Read, limit: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    reader
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > limit {
        return Err(OUTPUT_LIMIT.into());
    }
    Ok(out)
}

pub fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zstd_decompress_limited(data, usize::MAX)
}
/// [`zstd_decompress`], failing with [`OUTPUT_LIMIT`] once the output passes `limit` bytes.
#[cfg(not(any(target_arch = "wasm32", target_os = "none")))]
pub fn zstd_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut decoder = zstd::stream::read::Decoder::new(data).map_err(|e| e.to_string())?;
    read_limited(&mut decoder, limit)
}
#[cfg(any(target_arch = "wasm32", target_os = "none"))]
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}
#[cfg(any(target_arch = "wasm32", target_os = "none"))]
pub fn zstd_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| e.to_string())?;
    read_limited(&mut decoder, limit)
}

/// A Brotli compressor driven like `BrotliEncoderCompressStream`.
pub struct BrotliEncoder {
    state: BrotliEncoderStateStruct<StandardAlloc>,
    params: Vec<(u32, u32)>,
}

fn brotli_param(id: u32) -> Option<BrotliEncoderParameter> {
    use BrotliEncoderParameter::*;
    Some(match id {
        0 => BROTLI_PARAM_MODE,
        1 => BROTLI_PARAM_QUALITY,
        2 => BROTLI_PARAM_LGWIN,
        3 => BROTLI_PARAM_LGBLOCK,
        4 => BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING,
        5 => BROTLI_PARAM_SIZE_HINT,
        6 => BROTLI_PARAM_LARGE_WINDOW,
        _ => return None,
    })
}

impl BrotliEncoder {
    /// `params` are `(BROTLI_PARAM_*, value)` pairs; parameters the encoder lacks are ignored.
    pub fn new(params: &[(u32, u32)]) -> Self {
        let mut state = BrotliEncoderStateStruct::new(StandardAlloc::default());
        for &(id, value) in params {
            if let Some(p) = brotli_param(id) {
                state.set_parameter(p, value);
            }
        }
        BrotliEncoder {
            state,
            params: params.to_vec(),
        }
    }

    pub fn reset(&mut self) {
        *self = BrotliEncoder::new(&self.params.clone());
    }

    /// One `BrotliEncoderCompressStream` step; `op` is a `BROTLI_OPERATION_*`. `None` when the
    /// encoder failed. Returns `(consumed, produced)`.
    pub fn run(&mut self, op: u32, input: &[u8], output: &mut [u8]) -> Option<(usize, usize)> {
        let op = match op {
            0 => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
            1 => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
            2 => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
            _ => BrotliEncoderOperation::BROTLI_OPERATION_EMIT_METADATA,
        };
        let (mut avail_in, mut in_off) = (input.len(), 0);
        let (mut avail_out, mut out_off) = (output.len(), 0);
        let ok = self.state.compress_stream(
            op,
            &mut avail_in,
            input,
            &mut in_off,
            &mut avail_out,
            output,
            &mut out_off,
            &mut None,
            &mut |_, _, _, _| (),
        );
        ok.then_some((in_off, out_off))
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    pub fn has_more_output(&self) -> bool {
        self.state.has_more_output()
    }
}

/// Compress `data` into one complete Brotli stream at `quality` (0..=11) with a `1 << lgwin`
/// byte window.
pub fn brotli_compress(data: &[u8], quality: u32, lgwin: u32) -> Vec<u8> {
    const PARAM_QUALITY: u32 = 1;
    const PARAM_LGWIN: u32 = 2;
    const OPERATION_FINISH: u32 = 2;
    let mut enc = BrotliEncoder::new(&[(PARAM_QUALITY, quality), (PARAM_LGWIN, lgwin)]);
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let mut consumed = 0;
    while !enc.is_finished() {
        let start = out.len();
        out.resize(start + 65536, 0);
        let (used, produced) = enc
            .run(OPERATION_FINISH, &data[consumed..], &mut out[start..])
            .expect("brotli encoder failed");
        consumed += used;
        out.truncate(start + produced);
    }
    out
}

/// How a [`BrotliDecoder::run`] step ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrotliStatus {
    Success,
    NeedsMoreInput,
    NeedsMoreOutput,
    /// The decoder failed with this `BROTLI_DECODER_ERROR_*` code.
    Error(i32),
}

type DecoderState = BrotliState<StandardAlloc, StandardAlloc, HeapAlloc<HuffmanCode>>;

fn new_decoder_state() -> DecoderState {
    BrotliState::new_strict(
        StandardAlloc::default(),
        StandardAlloc::default(),
        HeapAlloc::<HuffmanCode>::new(HuffmanCode { bits: 2, value: 1 }),
    )
}

/// Decode a complete Brotli stream, failing with [`OUTPUT_LIMIT`] as soon as the output passes
/// `limit` bytes.
pub fn brotli_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut dec = BrotliDecoder::new();
    let mut out = Vec::new();
    let mut consumed = 0;
    loop {
        let start = out.len();
        out.resize(start + step_window(start, 65536, limit), 0);
        let (status, used, produced) = dec.run(&data[consumed..], &mut out[start..]);
        consumed += used;
        out.truncate(start + produced);
        if out.len() > limit {
            return Err(OUTPUT_LIMIT.into());
        }
        match status {
            BrotliStatus::Success => return Ok(out),
            BrotliStatus::NeedsMoreOutput => {}
            BrotliStatus::NeedsMoreInput => return Err("brotli: unexpected end of input".into()),
            BrotliStatus::Error(code) => return Err(format!("brotli: decoder error {code}")),
        }
    }
}

/// A Brotli decompressor driven like `BrotliDecoderDecompressStream`.
pub struct BrotliDecoder {
    state: DecoderState,
    total_out: usize,
}

impl Default for BrotliDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliDecoder {
    pub fn new() -> Self {
        BrotliDecoder {
            state: new_decoder_state(),
            total_out: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = BrotliDecoder::new();
    }

    /// Returns the step's status with `(consumed, produced)`.
    pub fn run(&mut self, input: &[u8], output: &mut [u8]) -> (BrotliStatus, usize, usize) {
        let (mut avail_in, mut in_off) = (input.len(), 0);
        let (mut avail_out, mut out_off) = (output.len(), 0);
        let result = brotli::BrotliDecompressStream(
            &mut avail_in,
            &mut in_off,
            input,
            &mut avail_out,
            &mut out_off,
            output,
            &mut self.total_out,
            &mut self.state,
        );
        let status = match result {
            BrotliResult::ResultSuccess => BrotliStatus::Success,
            BrotliResult::NeedsMoreInput => BrotliStatus::NeedsMoreInput,
            BrotliResult::NeedsMoreOutput => BrotliStatus::NeedsMoreOutput,
            BrotliResult::ResultFailure => BrotliStatus::Error(self.state.error_code as i32),
        };
        (status, in_off, out_off)
    }
}
