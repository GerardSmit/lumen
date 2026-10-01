//! Compression codecs for the runtime: safe wrappers over maintained crates (zlib-rs through
//! `libz-rs-sys`, `brotli`, `zstd`). The wrappers keep the streaming shape of the C libraries
//! (explicit input/output windows, flush modes, status codes) so `node:zlib` can reproduce
//! zlib's and Brotli's exact write semantics on top of them; the one-shot helpers serve
//! `CompressionStream`, `Bun.zstd*` and friends.

use std::ffi::c_int;

use brotli::enc::encode::{BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct};
use brotli::enc::StandardAlloc;
use brotli::{BrotliResult, BrotliState, HeapAlloc, HuffmanCode};
use libz_rs_sys as z;

pub const Z_OK: i32 = 0;
pub const Z_STREAM_END: i32 = 1;
pub const Z_NEED_DICT: i32 = 2;
pub const Z_STREAM_ERROR: i32 = -2;
pub const Z_DATA_ERROR: i32 = -3;
pub const Z_BUF_ERROR: i32 = -5;
pub const Z_FINISH: i32 = 4;

/// What one call to [`ZStream::run`] did: zlib's return code plus how much of the input window
/// was consumed and how much of the output window was filled.
#[derive(Clone, Copy, Debug)]
pub struct ZStep {
    pub code: i32,
    pub consumed: usize,
    pub produced: usize,
}

/// A zlib `z_stream` (deflate or inflate direction), owned and pinned on the heap.
pub struct ZStream {
    strm: Box<z::z_stream>,
    deflating: bool,
}

fn stream_size() -> c_int {
    std::mem::size_of::<z::z_stream>() as c_int
}

impl ZStream {
    /// `deflateInit2`; `window_bits` carries zlib's wrapper convention (negative: raw, +16: gzip).
    pub fn deflate(level: i32, window_bits: i32, mem_level: i32, strategy: i32) -> Result<Self, i32> {
        let mut strm = Box::new(z::z_stream::default());
        // SAFETY: `strm` is a live, default-initialised stream with its allocator set.
        let code = unsafe {
            z::deflateInit2_(&mut *strm, level, 8, window_bits, mem_level, strategy, z::zlibVersion(), stream_size())
        };
        if code != Z_OK {
            return Err(code);
        }
        Ok(ZStream { strm, deflating: true })
    }

    /// `inflateInit2`; `window_bits` as for [`ZStream::deflate`] (+32: auto-detect zlib or gzip).
    pub fn inflate(window_bits: i32) -> Result<Self, i32> {
        let mut strm = Box::new(z::z_stream::default());
        // SAFETY: as for `deflate`.
        let code = unsafe { z::inflateInit2_(&mut *strm, window_bits, z::zlibVersion(), stream_size()) };
        if code != Z_OK {
            return Err(code);
        }
        Ok(ZStream { strm, deflating: false })
    }

    pub fn run(&mut self, flush: i32, input: &[u8], output: &mut [u8]) -> ZStep {
        let s = &mut *self.strm;
        s.next_in = input.as_ptr();
        s.avail_in = input.len().min(u32::MAX as usize) as u32;
        s.next_out = output.as_mut_ptr();
        s.avail_out = output.len().min(u32::MAX as usize) as u32;
        let (in_before, out_before) = (s.avail_in, s.avail_out);
        // SAFETY: the windows point into live slices of at least the advertised lengths.
        let code = unsafe {
            if self.deflating {
                z::deflate(s, flush)
            } else {
                z::inflate(s, flush)
            }
        };
        let step = ZStep {
            code,
            consumed: (in_before - s.avail_in) as usize,
            produced: (out_before - s.avail_out) as usize,
        };
        s.next_in = std::ptr::null();
        s.next_out = std::ptr::null_mut();
        s.avail_in = 0;
        s.avail_out = 0;
        step
    }

    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> i32 {
        // SAFETY: the dictionary slice is live for the call.
        unsafe {
            if self.deflating {
                z::deflateSetDictionary(&mut *self.strm, dictionary.as_ptr(), dictionary.len() as u32)
            } else {
                z::inflateSetDictionary(&mut *self.strm, dictionary.as_ptr(), dictionary.len() as u32)
            }
        }
    }

    pub fn params(&mut self, level: i32, strategy: i32) -> i32 {
        // SAFETY: the stream is initialised; both windows are empty.
        unsafe { z::deflateParams(&mut *self.strm, level, strategy) }
    }

    pub fn reset(&mut self) -> i32 {
        // SAFETY: the stream is initialised.
        unsafe {
            if self.deflating {
                z::deflateReset(&mut *self.strm)
            } else {
                z::inflateReset(&mut *self.strm)
            }
        }
    }

    /// The library's description of the last error, if it set one.
    pub fn message(&self) -> Option<String> {
        if self.strm.msg.is_null() {
            return None;
        }
        // SAFETY: zlib's `msg` is a NUL-terminated static string.
        let text = unsafe { std::ffi::CStr::from_ptr(self.strm.msg) };
        Some(text.to_string_lossy().into_owned())
    }

    #[allow(clippy::unnecessary_cast)] // `uLong` is 32-bit on some targets
    pub fn total_in(&self) -> u64 {
        self.strm.total_in as u64
    }
}

impl Drop for ZStream {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by `deflate`/`inflate` and is ended exactly once.
        unsafe {
            if self.deflating {
                z::deflateEnd(&mut *self.strm);
            } else {
                z::inflateEnd(&mut *self.strm);
            }
        }
    }
}

#[inline]
pub fn crc32_from(seed: u32, data: &[u8]) -> u32 {
    // SAFETY: the pointer and length describe a live slice.
    unsafe { z::crc32_z(seed as _, data.as_ptr(), data.len()) as u32 }
}

fn compress_with(window_bits: i32, data: &[u8]) -> Vec<u8> {
    let mut z = ZStream::deflate(-1, window_bits, 8, 0).expect("deflate init");
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    let mut consumed = 0;
    loop {
        let start = out.len();
        out.resize(start + 16384.max(data.len() / 4), 0);
        let step = z.run(Z_FINISH, &data[consumed..], &mut out[start..]);
        consumed += step.consumed;
        out.truncate(start + step.produced);
        if step.code == Z_STREAM_END {
            return out;
        }
    }
}

/// The error every `*_limited` decoder returns once its output would pass the limit.
pub const OUTPUT_LIMIT: &str = "output exceeds the length limit";

/// Output window for the next decode step: one byte past the limit is the most a step may add,
/// so a stream that passes `limit` is caught without decoding further.
fn step_window(out_len: usize, want: usize, limit: usize) -> usize {
    want.min(limit.saturating_sub(out_len).saturating_add(1))
}

fn decompress_with(window_bits: i32, data: &[u8], what: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut z = ZStream::inflate(window_bits).map_err(|_| format!("{what}: init failed"))?;
    let mut out = Vec::with_capacity(data.len().saturating_mul(3).min(limit));
    let mut consumed = 0;
    loop {
        let start = out.len();
        out.resize(start + step_window(start, 16384.max(data.len()), limit), 0);
        let step = z.run(0, &data[consumed..], &mut out[start..]);
        consumed += step.consumed;
        out.truncate(start + step.produced);
        if out.len() > limit {
            return Err(OUTPUT_LIMIT.into());
        }
        match step.code {
            Z_STREAM_END => {
                return if consumed < data.len() {
                    Err(format!("{what}: trailing data after the end of the stream"))
                } else {
                    Ok(out)
                };
            }
            Z_OK => {}
            Z_BUF_ERROR if step.produced > 0 || consumed < data.len() => {}
            Z_BUF_ERROR => return Err(format!("{what}: unexpected end of input")),
            _ => {
                let reason = z.message().unwrap_or_else(|| "invalid data".to_string());
                return Err(format!("{what}: {reason}"));
            }
        }
    }
}

pub fn deflate(data: &[u8]) -> Vec<u8> {
    compress_with(-15, data)
}
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    compress_with(15, data)
}
pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    compress_with(31, data)
}
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    inflate_limited(data, usize::MAX)
}
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zlib_decompress_limited(data, usize::MAX)
}
pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    gzip_decompress_limited(data, usize::MAX)
}
/// [`inflate`], failing with [`OUTPUT_LIMIT`] as soon as the output passes `limit` bytes.
pub fn inflate_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    decompress_with(-15, data, "deflate", limit)
}
pub fn zlib_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    decompress_with(15, data, "zlib", limit)
}
pub fn gzip_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    decompress_with(31, data, "gzip", limit)
}

#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub fn zstd_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut decoder = zstd::stream::read::Decoder::new(data).map_err(|e| e.to_string())?;
    read_limited(&mut decoder, limit)
}
#[cfg(target_arch = "wasm32")]
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}
#[cfg(target_arch = "wasm32")]
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
        BrotliEncoder { state, params: params.to_vec() }
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
        BrotliDecoder { state: new_decoder_state(), total_out: 0 }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zlib_round_trips() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(zlib_decompress(&zlib_compress(&data)).unwrap(), data);
        assert_eq!(gzip_decompress(&gzip_compress(&data)).unwrap(), data);
        assert_eq!(inflate(&deflate(&data)).unwrap(), data);
        assert!(zlib_decompress(&zlib_compress(&data)[..20]).is_err());
    }

    #[test]
    fn limited_decoders_stop_past_the_limit() {
        let data = vec![7u8; 100_000];
        let err = Some(OUTPUT_LIMIT.to_string());
        for (packed, f) in [
            (zlib_compress(&data), zlib_decompress_limited as fn(&[u8], usize) -> _),
            (gzip_compress(&data), gzip_decompress_limited),
            (deflate(&data), inflate_limited),
            (zstd_compress(&data), zstd_decompress_limited),
        ] {
            assert_eq!(f(&packed, 99_999).err(), err);
            assert_eq!(f(&packed, 100_000).unwrap(), data);
        }
    }

    #[test]
    fn zstd_round_trips() {
        let data = b"hello hello hello hello".to_vec();
        assert_eq!(zstd_decompress(&zstd_compress(&data)).unwrap(), data);
    }

    #[test]
    fn brotli_round_trips() {
        let data = b"hello hello hello hello hello".to_vec();
        let mut enc = BrotliEncoder::new(&[]);
        let mut packed = vec![0u8; 256];
        let (consumed, produced) = enc.run(2, &data, &mut packed).unwrap();
        assert_eq!(consumed, data.len());
        assert!(enc.is_finished());
        let mut dec = BrotliDecoder::new();
        let mut out = vec![0u8; 256];
        let (status, _, n) = dec.run(&packed[..produced], &mut out);
        assert_eq!(status, BrotliStatus::Success);
        assert_eq!(&out[..n], &data[..]);
    }
}
