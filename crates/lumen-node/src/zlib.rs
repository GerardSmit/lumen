//! `internalBinding('zlib')` for node:zlib: the Zlib, BrotliEncoder and BrotliDecoder handles over
//! lumen-host's codec wrappers, reproducing Node's `node_zlib.cc` write contract — one codec call
//! per write over the caller's input and output windows, with the same error classification.

use std::collections::HashMap;

use lumen_host::codec::{
    self, BrotliDecoder, BrotliEncoder, BrotliStatus, ZStream, Z_BUF_ERROR, Z_DATA_ERROR, Z_FINISH,
    Z_NEED_DICT, Z_OK, Z_STREAM_END, Z_STREAM_ERROR,
};
use lumen_bind::{Data, NativeError};
use lumen_host::Ctx;

const DEFLATE: u32 = 1;
const INFLATE: u32 = 2;
const GZIP: u32 = 3;
const GUNZIP: u32 = 4;
const DEFLATERAW: u32 = 5;
const INFLATERAW: u32 = 6;
const UNZIP: u32 = 7;
const BROTLI_DECODE: u32 = 8;
const BROTLI_ENCODE: u32 = 9;

const GZIP_HEADER_ID1: u8 = 0x1f;
const GZIP_HEADER_ID2: u8 = 0x8b;
const BROTLI_OPERATION_FINISH: u32 = 2;

struct ZlibCtx {
    mode: u32,
    stream: Option<ZStream>,
    dictionary: Vec<u8>,
    init_error: Option<i32>,
    gzip_id_bytes_read: u8,
    window_bits: i32,
    level: i32,
    mem_level: i32,
    strategy: i32,
}

enum Handle {
    Zlib(ZlibCtx),
    BrotliEncode(BrotliEncoder),
    BrotliDecode(BrotliDecoder),
}

/// Live handles, keyed by the id JS holds; released by `handleClose` (the stream's `close()`, or
/// the collector finalizing its JS wrapper).
#[derive(Default)]
pub(crate) struct ZlibHandles {
    next: u64,
    handles: HashMap<u64, Handle>,
}

/// A failed write, in the shape of Node's `CompressionError`.
struct CodecError {
    message: String,
    errno: i32,
    code: String,
}

impl CodecError {
    fn zlib(message: &str, stream: Option<&ZStream>, errno: i32) -> Self {
        let message = stream
            .and_then(ZStream::message)
            .unwrap_or_else(|| message.to_string());
        CodecError { message, errno, code: zlib_code(errno) }
    }
}

fn zlib_code(errno: i32) -> String {
    match errno {
        0 => "Z_OK",
        1 => "Z_STREAM_END",
        2 => "Z_NEED_DICT",
        -1 => "Z_ERRNO",
        -2 => "Z_STREAM_ERROR",
        -3 => "Z_DATA_ERROR",
        -4 => "Z_MEM_ERROR",
        -5 => "Z_BUF_ERROR",
        -6 => "Z_VERSION_ERROR",
        other => return format!("Z_UNKNOWN_ERROR_{other}"),
    }
    .to_string()
}

impl ZlibCtx {
    fn new(mode: u32, window_bits: i32, level: i32, mem_level: i32, strategy: i32, dictionary: Vec<u8>) -> Self {
        let mut c = ZlibCtx {
            mode,
            stream: None,
            dictionary,
            init_error: None,
            gzip_id_bytes_read: 0,
            window_bits,
            level,
            mem_level,
            strategy,
        };
        c.init();
        c
    }

    fn wrapped_window_bits(&self) -> i32 {
        match self.mode {
            GZIP | GUNZIP => self.window_bits + 16,
            UNZIP => self.window_bits + 32,
            DEFLATERAW | INFLATERAW => -self.window_bits,
            _ => self.window_bits,
        }
    }

    fn deflates(&self) -> bool {
        matches!(self.mode, DEFLATE | GZIP | DEFLATERAW)
    }

    fn init(&mut self) {
        let window_bits = self.wrapped_window_bits();
        let created = if self.deflates() {
            ZStream::deflate(self.level, window_bits, self.mem_level, self.strategy)
        } else {
            ZStream::inflate(window_bits)
        };
        match created {
            Ok(stream) => {
                self.stream = Some(stream);
                self.apply_dictionary();
            }
            Err(code) => self.init_error = Some(code),
        }
    }

    // Deflaters take the dictionary up front; raw inflaters need it before any input. zlib and
    // gzip inflaters ask for it (`Z_NEED_DICT`) once their header names it.
    fn apply_dictionary(&mut self) {
        if self.dictionary.is_empty() || !matches!(self.mode, DEFLATE | DEFLATERAW | INFLATERAW) {
            return;
        }
        let Some(stream) = self.stream.as_mut() else { return };
        if stream.set_dictionary(&self.dictionary) != Z_OK {
            self.init_error = Some(Z_STREAM_ERROR);
        }
    }

    fn reset(&mut self) {
        self.gzip_id_bytes_read = 0;
        if let Some(stream) = self.stream.as_mut() {
            stream.reset();
        }
        self.apply_dictionary();
    }

    fn params(&mut self, level: i32, strategy: i32) -> Option<CodecError> {
        if !self.deflates() {
            return None;
        }
        let stream = self.stream.as_mut()?;
        let code = stream.params(level, strategy);
        if code != Z_OK && code != Z_BUF_ERROR {
            return Some(CodecError::zlib("Failed to set parameters", Some(stream), code));
        }
        self.level = level;
        self.strategy = strategy;
        None
    }

    /// One `ZlibContext::DoThreadPoolWork` + `GetErrorInfo`: returns `(availOut, availIn)`.
    fn write(&mut self, flush: i32, input: &[u8], out: &mut [u8]) -> Result<(usize, usize), CodecError> {
        if let Some(code) = self.init_error {
            return Err(CodecError::zlib("Init error", None, code));
        }
        let Some(mut stream) = self.stream.take() else {
            return Err(CodecError::zlib("zlib binding closed", None, Z_STREAM_ERROR));
        };
        let (code, consumed, produced) = self.process(&mut stream, flush, input, out);
        let avail_out = out.len() - produced;
        let result = self.classify(&stream, code, flush, avail_out);
        self.stream = Some(stream);
        result.map(|()| (avail_out, input.len() - consumed))
    }

    fn process(&mut self, stream: &mut ZStream, flush: i32, input: &[u8], out: &mut [u8]) -> (i32, usize, usize) {
        let first = stream.run(flush, input, out);
        if self.deflates() {
            return (first.code, first.consumed, first.produced);
        }
        let (mut code, mut consumed, mut produced) = (first.code, first.consumed, first.produced);
        if self.mode == UNZIP {
            self.detect_gzip(input);
        }
        if self.mode != INFLATERAW && code == Z_NEED_DICT && !self.dictionary.is_empty() {
            code = stream.set_dictionary(&self.dictionary);
            if code == Z_OK {
                let again = stream.run(flush, &input[consumed..], &mut out[produced..]);
                code = again.code;
                consumed += again.consumed;
                produced += again.produced;
            } else if code == Z_DATA_ERROR {
                // Both a bad dictionary and bad input report Z_DATA_ERROR; keep them apart.
                code = Z_NEED_DICT;
            }
        }
        // Further bytes after a gunzip member are another member, or padding if they are zeros.
        while consumed < input.len() && self.mode == GUNZIP && code == Z_STREAM_END && input[consumed] != 0 {
            stream.reset();
            let next = stream.run(flush, &input[consumed..], &mut out[produced..]);
            code = next.code;
            consumed += next.consumed;
            produced += next.produced;
        }
        (code, consumed, produced)
    }

    // An UNZIP stream is a gunzip stream only when it opens with the gzip magic.
    fn detect_gzip(&mut self, input: &[u8]) {
        let mut rest = input;
        if self.gzip_id_bytes_read == 0 {
            match rest.first() {
                None => return,
                Some(&GZIP_HEADER_ID1) => {
                    self.gzip_id_bytes_read = 1;
                    rest = &rest[1..];
                }
                Some(_) => {
                    self.mode = INFLATE;
                    return;
                }
            }
        }
        if self.gzip_id_bytes_read == 1 {
            match rest.first() {
                None => {}
                Some(&GZIP_HEADER_ID2) => {
                    self.gzip_id_bytes_read = 2;
                    self.mode = GUNZIP;
                }
                Some(_) => self.mode = INFLATE,
            }
        }
    }

    fn classify(&self, stream: &ZStream, code: i32, flush: i32, avail_out: usize) -> Result<(), CodecError> {
        match code {
            Z_OK | Z_BUF_ERROR => {
                if avail_out != 0 && flush == Z_FINISH {
                    return Err(CodecError::zlib("unexpected end of file", Some(stream), code));
                }
                Ok(())
            }
            Z_STREAM_END => Ok(()),
            Z_NEED_DICT => {
                let message = if self.dictionary.is_empty() { "Missing dictionary" } else { "Bad dictionary" };
                Err(CodecError::zlib(message, Some(stream), code))
            }
            _ => Err(CodecError::zlib("Zlib error", Some(stream), code)),
        }
    }
}

fn brotli_decoder_error_code(errno: i32) -> String {
    let name = match errno {
        -1 => "FORMAT_EXUBERANT_NIBBLE",
        -2 => "FORMAT_RESERVED",
        -3 => "FORMAT_EXUBERANT_META_NIBBLE",
        -4 => "FORMAT_SIMPLE_HUFFMAN_ALPHABET",
        -5 => "FORMAT_SIMPLE_HUFFMAN_SAME",
        -6 => "FORMAT_CL_SPACE",
        -7 => "FORMAT_HUFFMAN_SPACE",
        -8 => "FORMAT_CONTEXT_MAP_REPEAT",
        -9 => "FORMAT_BLOCK_LENGTH_1",
        -10 => "FORMAT_BLOCK_LENGTH_2",
        -11 => "FORMAT_TRANSFORM",
        -12 => "FORMAT_DICTIONARY",
        -13 => "FORMAT_WINDOW_BITS",
        -14 => "FORMAT_PADDING_1",
        -15 => "FORMAT_PADDING_2",
        -16 => "FORMAT_DISTANCE",
        -19 => "DICTIONARY_NOT_SET",
        -20 => "INVALID_ARGUMENTS",
        -21 => "ALLOC_CONTEXT_MODES",
        -22 => "ALLOC_TREE_GROUPS",
        -25 => "ALLOC_CONTEXT_MAP",
        -26 => "ALLOC_RING_BUFFER_1",
        -27 => "ALLOC_RING_BUFFER_2",
        -30 => "ALLOC_BLOCK_TYPE_TREES",
        _ => "UNREACHABLE",
    };
    format!("ERR__ERROR_{name}")
}

fn brotli_encode_write(enc: &mut BrotliEncoder, op: u32, input: &[u8], out: &mut [u8]) -> Result<(usize, usize), CodecError> {
    match enc.run(op, input, out) {
        Some((consumed, produced)) => Ok((out.len() - produced, input.len() - consumed)),
        None => Err(CodecError {
            message: "Compression failed".into(),
            errno: -1,
            code: "ERR_BROTLI_COMPRESSION_FAILED".into(),
        }),
    }
}

fn brotli_decode_write(dec: &mut BrotliDecoder, op: u32, input: &[u8], out: &mut [u8]) -> Result<(usize, usize), CodecError> {
    let (status, consumed, produced) = dec.run(input, out);
    match status {
        BrotliStatus::Error(errno) => Err(CodecError {
            message: "Decompression failed".into(),
            errno,
            code: brotli_decoder_error_code(errno),
        }),
        BrotliStatus::NeedsMoreInput if op == BROTLI_OPERATION_FINISH => Err(CodecError {
            message: "unexpected end of file".into(),
            errno: Z_BUF_ERROR,
            code: zlib_code(Z_BUF_ERROR),
        }),
        _ => Ok((out.len() - produced, input.len() - consumed)),
    }
}

fn u32_pairs(bytes: &[u8]) -> Vec<(u32, u32)> {
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect();
    words.chunks_exact(2).map(|p| (p[0], p[1])).collect()
}

pub(crate) use bindings::Module;

fn error_data(e: CodecError) -> Data {
    Data::List(vec![Data::Str(e.message), Data::Int(e.errno as i64), Data::Str(e.code)])
}

#[lumen_bind::module(name = "__zlib")]
mod bindings {
    use super::*;

    fn register(ctx: &mut Ctx, handle: Handle) -> f64 {
        let reg = ctx.host_mut::<ZlibHandles>().expect("registry");
        reg.next += 1;
        let id = reg.next;
        reg.handles.insert(id, handle);
        id as f64
    }

    /// `(mode, windowBits, level, memLevel, strategy, dictionary?)` for the zlib modes (Node's
    /// `DEFLATE` = 1 .. `UNZIP` = 7); returns the handle id.
    #[op(name = "handleOpen")]
    fn op_handle_open(
        ctx: &mut Ctx,
        mode: f64,
        window_bits: Option<f64>,
        level: Option<f64>,
        mem_level: Option<f64>,
        strategy: Option<f64>,
        dictionary: Option<Vec<u8>>,
    ) -> Result<f64, NativeError> {
        let mode = mode as u32;
        if !(DEFLATE..=UNZIP).contains(&mode) {
            return Err(NativeError::type_error(format!("unknown zlib mode {mode}")));
        }
        let handle = Handle::Zlib(ZlibCtx::new(
            mode,
            window_bits.unwrap_or(15.0) as i32,
            level.unwrap_or(-1.0) as i32,
            mem_level.unwrap_or(8.0) as i32,
            strategy.unwrap_or(0.0) as i32,
            dictionary.unwrap_or_default(),
        ));
        Ok(register(ctx, handle))
    }

    /// `(mode, pairs?)` for Brotli: `pairs` is a Uint32Array of `[parameter, value]` pairs for the
    /// encoder; returns the handle id.
    #[op(name = "brotliOpen")]
    fn op_brotli_open(ctx: &mut Ctx, mode: f64, pairs: Option<Vec<u8>>) -> Result<f64, NativeError> {
        let handle = match mode as u32 {
            BROTLI_ENCODE => Handle::BrotliEncode(BrotliEncoder::new(&u32_pairs(&pairs.unwrap_or_default()))),
            BROTLI_DECODE => Handle::BrotliDecode(BrotliDecoder::new()),
            other => return Err(NativeError::type_error(format!("unknown zlib mode {other}"))),
        };
        Ok(register(ctx, handle))
    }

    /// `(id, flush, in, inOff, inLen, out, outOff, outLen)` - one codec step over the given
    /// windows, filling `out` in place. Returns `[availOutAfter, availInAfter]`, or
    /// `[message, errno, code]` when the codec failed.
    #[op(name = "handleWrite")]
    #[allow(clippy::too_many_arguments)]
    fn op_handle_write(
        ctx: &mut Ctx,
        id: f64,
        flush: f64,
        input: Option<&[u8]>,
        in_off: f64,
        in_len: f64,
        out: &mut [u8],
        out_off: f64,
        out_len: f64,
    ) -> Result<Data, NativeError> {
        let (in_off, in_len, out_off, out_len) = (in_off as usize, in_len as usize, out_off as usize, out_len as usize);
        if out_off.checked_add(out_len).is_none_or(|end| end > out.len()) {
            return Err(NativeError::value_error("zlib output window out of range"));
        }
        let input: &[u8] = match input {
            Some(bytes) => {
                if in_off.checked_add(in_len).is_none_or(|end| end > bytes.len()) {
                    return Err(NativeError::value_error("zlib input window out of range"));
                }
                &bytes[in_off..in_off + in_len]
            }
            None => &[],
        };
        let out = &mut out[out_off..out_off + out_len];
        let flush = flush as i32;
        let Some(handle) = ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&(id as u64))) else {
            let e = CodecError { message: "zlib binding closed".into(), errno: Z_STREAM_ERROR, code: zlib_code(Z_STREAM_ERROR) };
            return Ok(error_data(e));
        };
        let op = flush as u32;
        let result = match handle {
            Handle::Zlib(c) => c.write(flush, input, out),
            Handle::BrotliEncode(enc) => brotli_encode_write(enc, op, input, out),
            Handle::BrotliDecode(dec) => brotli_decode_write(dec, op, input, out),
        };
        Ok(match result {
            Ok((avail_out, avail_in)) => Data::List(vec![Data::Int(avail_out as i64), Data::Int(avail_in as i64)]),
            Err(e) => error_data(e),
        })
    }

    /// `(id, level, strategy)` - zlib's `deflateParams`; returns `[message, errno, code]` on failure.
    #[op(name = "handleParams")]
    fn op_handle_params(ctx: &mut Ctx, id: f64, level: Option<f64>, strategy: Option<f64>) -> Data {
        let (level, strategy) = (level.unwrap_or(-1.0) as i32, strategy.unwrap_or(0.0) as i32);
        let error = match ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&(id as u64))) {
            Some(Handle::Zlib(c)) => c.params(level, strategy),
            _ => None,
        };
        error.map_or(Data::None, error_data)
    }

    /// `(id)` - `zlib.reset()`: a fresh stream with the same options.
    #[op(name = "handleReset")]
    fn op_handle_reset(ctx: &mut Ctx, id: f64) {
        match ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&(id as u64))) {
            Some(Handle::Zlib(c)) => c.reset(),
            Some(Handle::BrotliEncode(enc)) => enc.reset(),
            Some(Handle::BrotliDecode(dec)) => dec.reset(),
            None => {}
        }
    }

    #[op(name = "handleClose")]
    fn op_handle_close(ctx: &mut Ctx, id: f64) {
        if let Some(reg) = ctx.host_mut::<ZlibHandles>() {
            reg.handles.remove(&(id as u64));
        }
    }

    #[op(name = "zstdCompress")]
    fn op_zstd_compress(bytes: &[u8]) -> Vec<u8> {
        codec::zstd_compress(bytes)
    }

    #[op(name = "zstdDecompress")]
    fn op_zstd_decompress(bytes: &[u8]) -> Result<Vec<u8>, NativeError> {
        codec::zstd_decompress(bytes).map_err(NativeError::runtime)
    }

    /// `(bytes, seed)` - CRC-32 of `bytes`, optionally continued from `seed`.
    #[op(name = "crc32")]
    fn op_crc32(bytes: &[u8], seed: Option<f64>) -> u32 {
        codec::crc32_from(seed.unwrap_or(0.0) as u32, bytes)
    }
}
