//! `internalBinding('zlib')` for node:zlib: the Zlib, BrotliEncoder and BrotliDecoder handles over
//! lumen-host's codec wrappers, reproducing Node's `node_zlib.cc` write contract — one codec call
//! per write over the caller's input and output windows, with the same error classification.

use std::collections::HashMap;

use lumen_host::codec::{
    self, BrotliDecoder, BrotliEncoder, BrotliStatus, ZStream, Z_BUF_ERROR, Z_DATA_ERROR, Z_FINISH,
    Z_NEED_DICT, Z_OK, Z_STREAM_END, Z_STREAM_ERROR,
};
use lumen_host::{Ctx, Value};

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

fn arg_num(a: &[Value], i: usize, default: f64) -> f64 {
    a.get(i).and_then(Value::as_num_opt).unwrap_or(default)
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

/// `(mode, windowBits, level, memLevel, strategy, dictionary?)` for the zlib modes (Node's
/// `DEFLATE` = 1 … `UNZIP` = 7), or `(mode, Uint32Array of [parameter, value] pairs)` for Brotli;
/// returns the handle id.
pub(crate) fn op_handle_open(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let mode = arg_num(a, 0, 0.0) as u32;
    let handle = match mode {
        BROTLI_ENCODE => {
            let pairs = a.get(1).and_then(|v| ctx.typed_array_bytes(v)).map(|b| u32_pairs(&b));
            Handle::BrotliEncode(BrotliEncoder::new(&pairs.unwrap_or_default()))
        }
        BROTLI_DECODE => Handle::BrotliDecode(BrotliDecoder::new()),
        DEFLATE..=UNZIP => {
            let dictionary = a.get(5).and_then(|v| ctx.typed_array_bytes(v)).unwrap_or_default();
            Handle::Zlib(ZlibCtx::new(
                mode,
                arg_num(a, 1, 15.0) as i32,
                arg_num(a, 2, -1.0) as i32,
                arg_num(a, 3, 8.0) as i32,
                arg_num(a, 4, 0.0) as i32,
                dictionary,
            ))
        }
        _ => return Err(ctx.make_error("TypeError", format!("unknown zlib mode {mode}"))),
    };
    let reg = ctx.host_mut::<ZlibHandles>().expect("registry");
    reg.next += 1;
    let id = reg.next;
    reg.handles.insert(id, handle);
    Ok(Value::Num(id as f64))
}

fn handle_id(a: &[Value]) -> u64 {
    a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64
}

fn error_value(ctx: &mut Ctx, e: CodecError) -> Value {
    ctx.make_array(vec![Value::from_string(e.message), Value::Num(e.errno as f64), Value::from_string(e.code)])
}

/// `(id, flush, in, inOff, inLen, out, outOff, outLen)` — one codec step over the given windows,
/// filling `out` in place. Returns `[availOutAfter, availInAfter]`, or `[message, errno, code]`
/// when the codec failed.
pub(crate) fn op_handle_write(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = handle_id(a);
    let flush = arg_num(a, 1, 0.0) as i32;
    let (in_off, in_len) = (arg_num(a, 3, 0.0) as usize, arg_num(a, 4, 0.0) as usize);
    let (out_off, out_len) = (arg_num(a, 6, 0.0) as usize, arg_num(a, 7, 0.0) as usize);
    let in_raw = a.get(2).and_then(|v| ctx.typed_array_raw(v));
    let out_raw = a.get(5).and_then(|v| ctx.typed_array_raw(v));
    let Some((_, out_total, out_ptr)) = out_raw else {
        return Err(ctx.make_error("TypeError", "zlib expects Buffer/TypedArray windows"));
    };
    if out_off.checked_add(out_len).is_none_or(|end| end > out_total) {
        return Err(ctx.make_error("RangeError", "zlib output window out of range"));
    }
    let input: &[u8] = match in_raw {
        Some((_, total, ptr)) => {
            if in_off.checked_add(in_len).is_none_or(|end| end > total) {
                return Err(ctx.make_error("RangeError", "zlib input window out of range"));
            }
            // SAFETY: the window lies inside the live typed array, which nothing resizes during
            // this synchronous call.
            unsafe { std::slice::from_raw_parts(ptr.add(in_off), in_len) }
        }
        None => &[],
    };
    // SAFETY: as above; input and output are distinct buffers (the caller owns both).
    let out = unsafe { std::slice::from_raw_parts_mut(out_ptr.add(out_off), out_len) };
    let Some(handle) = ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&id)) else {
        let e = CodecError { message: "zlib binding closed".into(), errno: Z_STREAM_ERROR, code: zlib_code(Z_STREAM_ERROR) };
        return Ok(error_value(ctx, e));
    };
    let op = flush as u32;
    let result = match handle {
        Handle::Zlib(c) => c.write(flush, input, out),
        Handle::BrotliEncode(enc) => brotli_encode_write(enc, op, input, out),
        Handle::BrotliDecode(dec) => brotli_decode_write(dec, op, input, out),
    };
    match result {
        Ok((avail_out, avail_in)) => Ok(ctx.make_array(vec![Value::Num(avail_out as f64), Value::Num(avail_in as f64)])),
        Err(e) => Ok(error_value(ctx, e)),
    }
}

/// `(id, level, strategy)` — zlib's `deflateParams`; returns `[message, errno, code]` on failure.
pub(crate) fn op_handle_params(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = handle_id(a);
    let (level, strategy) = (arg_num(a, 1, -1.0) as i32, arg_num(a, 2, 0.0) as i32);
    let error = match ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&id)) {
        Some(Handle::Zlib(c)) => c.params(level, strategy),
        _ => None,
    };
    Ok(match error {
        Some(e) => error_value(ctx, e),
        None => Value::Undefined,
    })
}

/// `(id)` — `zlib.reset()`: a fresh stream with the same options.
pub(crate) fn op_handle_reset(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = handle_id(a);
    match ctx.host_mut::<ZlibHandles>().and_then(|r| r.handles.get_mut(&id)) {
        Some(Handle::Zlib(c)) => c.reset(),
        Some(Handle::BrotliEncode(enc)) => enc.reset(),
        Some(Handle::BrotliDecode(dec)) => dec.reset(),
        None => {}
    }
    Ok(Value::Undefined)
}

pub(crate) fn op_handle_close(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = handle_id(a);
    if let Some(reg) = ctx.host_mut::<ZlibHandles>() {
        reg.handles.remove(&id);
    }
    Ok(Value::Undefined)
}

fn bytes_arg(ctx: &mut Ctx, a: &[Value]) -> Result<Vec<u8>, Value> {
    match a.first().and_then(|v| ctx.typed_array_bytes(v)) {
        Some(bytes) => Ok(bytes),
        None => Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray")),
    }
}

pub(crate) fn op_zstd_compress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = bytes_arg(ctx, a)?;
    ctx.make_uint8array(&codec::zstd_compress(&bytes))
}

pub(crate) fn op_zstd_decompress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = bytes_arg(ctx, a)?;
    match codec::zstd_decompress(&bytes) {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `__zlib.crc32(bytes, seed)` — CRC-32 of `bytes`, optionally continued from `seed`.
pub(crate) fn op_crc32(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = bytes_arg(ctx, a)?;
    let seed = arg_num(a, 1, 0.0) as u32;
    Ok(Value::Num(codec::crc32_from(seed, &bytes) as f64))
}
