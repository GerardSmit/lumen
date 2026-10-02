//! `zlib` on the shared zlib stream in `lumen_common::compress`, with CPython's buffer, flush and
//! error semantics (`Modules/zlibmodule.c`).

/// The functions in this module allow compression and decompression using the
/// zlib library, which is based on GNU zip.
///
/// adler32(string[, start]) -- Compute an Adler-32 checksum.
/// compress(data[, level]) -- Compress data, with compression level 0-9 or -1.
/// compressobj([level[, ...]]) -- Return a compressor object.
/// crc32(string[, start]) -- Compute a CRC-32 checksum.
/// decompress(string,[wbits],[bufsize]) -- Decompresses a compressed string.
/// decompressobj([wbits[, zdict]]) -- Return a decompressor object.
///
/// 'wbits' is window buffer size and container format.
/// Compressor objects support compress() and flush() methods; decompressor
/// objects support decompress() and flush().
#[lumen_bind::module(name = "zlib")]
pub mod zlib {
    #![allow(clippy::new_ret_no_self)]

    use crate::bind::{opaque_instance, Py, This};
    use crate::builtins::decompressor::{Chunk, Codec, Decompressor, Failure};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::compress::{self as zc, ZStream, Z_BUF_ERROR, Z_FINISH, Z_NEED_DICT, Z_OK, Z_STREAM_END, Z_STREAM_ERROR};

    const Z_NO_FLUSH: i32 = 0;
    const Z_SYNC_FLUSH: i32 = 2;
    const MAX_WBITS: i64 = 15;
    const DEF_BUF_SIZE: usize = 16 * 1024;
    const DEF_MEM_LEVEL: i32 = 8;
    /// The first block CPython's output buffer allocates when no size is requested.
    const FIRST_BLOCK: usize = 32 * 1024;
    const MAX_INITIAL_BUF: usize = 16 * 1024 * 1024;

    #[derive(Default)]
    pub struct State {
        error: Option<Obj>,
    }

    fn error(it: &mut Interp, msg: String) -> Obj {
        let cls = match it.native_state::<State>().error.clone() {
            Some(c) => c,
            None => it.exc_type("Exception"),
        };
        it.new_exc(&cls, vec![Value::string(msg)])
    }

    /// `zlib_error`: "Error <code> <msg>: <zlib's message>".
    fn zlib_error(it: &mut Interp, z: Option<&ZStream>, code: i32, msg: &str) -> Obj {
        let zmsg = z.and_then(|z| z.message()).or_else(|| {
            match code {
                Z_BUF_ERROR => Some("incomplete or truncated stream"),
                Z_STREAM_ERROR => Some("inconsistent stream state"),
                -3 => Some("invalid input data"),
                _ => None,
            }
            .map(str::to_string)
        });
        match zmsg {
            Some(m) => error(it, format!("Error {code} {msg}: {m}")),
            None => error(it, format!("Error {code} {msg}")),
        }
    }

    fn buffer(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
        it.buffer_bytes(v)
    }

    fn c_int(it: &mut Interp, v: i64) -> R<i32> {
        i32::try_from(v).map_err(|_| {
            let msg = if v < 0 { "signed integer is less than minimum" } else { "signed integer is greater than maximum" };
            it.new_exc_str("OverflowError", msg)
        })
    }

    /// The low 32 bits of an int (`unsigned_int(bitwise=True)`).
    fn bitwise_u32(it: &mut Interp, v: &Value) -> R<u32> {
        let n = it.call_method(v, "__and__", vec![Value::Int(0xffff_ffff)])?;
        Ok(it.index_of(&n)? as u32)
    }

    /// What one CPython inflate/deflate loop did.
    struct Run {
        out: Vec<u8>,
        consumed: usize,
        code: i32,
    }

    /// Runs `z` over `input` until it stops filling the output window (all input consumed or
    /// the stream ended), the output reaches `max`, or zlib reports an error. A `Z_NEED_DICT`
    /// is answered with `zdict` when one is given.
    fn drive(z: &mut ZStream, input: &[u8], flush: i32, first: usize, max: Option<usize>, zdict: Option<&[u8]>) -> Run {
        let mut out: Vec<u8> = Vec::new();
        let (mut produced, mut consumed, mut code) = (0, 0, Z_OK);
        loop {
            if produced == out.len() {
                if max.is_some_and(|m| produced >= m) {
                    break;
                }
                let grow = if out.is_empty() { first.max(1) } else { out.len() };
                let len = max.map_or(out.len() + grow, |m| (out.len() + grow).min(m));
                out.resize(len, 0);
            }
            let step = z.run(flush, &input[consumed..], &mut out[produced..]);
            consumed += step.consumed;
            produced += step.produced;
            code = step.code;
            if code == Z_NEED_DICT {
                match zdict {
                    Some(d) => {
                        code = z.set_dictionary(d);
                        if code != Z_OK {
                            break;
                        }
                        continue;
                    }
                    None => break,
                }
            }
            if !matches!(code, Z_OK | Z_BUF_ERROR | Z_STREAM_END) || code == Z_STREAM_END || produced < out.len() {
                break;
            }
        }
        out.truncate(produced);
        Run { out, consumed, code }
    }

    /// Returns a bytes object containing compressed data.
    ///
    ///   data
    ///     Binary data to be compressed.
    ///   level
    ///     Compression level, in 0-9 or -1.
    ///   wbits
    ///     The window buffer size and container format.
    #[op(hint(py(text_signature = "($module, data, /, level=Z_DEFAULT_COMPRESSION, wbits=MAX_WBITS)")))]
    fn compress(it: &mut Interp, data: &Value, #[kw] #[default(-1)] level: i64, #[kw] #[default(15)] wbits: i64) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        let (level, wbits) = (c_int(it, level)?, c_int(it, wbits)?);
        let mut z = match ZStream::deflate(level, wbits, DEF_MEM_LEVEL, 0) {
            Ok(z) => z,
            Err(Z_STREAM_ERROR) => return Err(error(it, "Bad compression level".into())),
            Err(code) => return Err(zlib_error(it, None, code, "while compressing data")),
        };
        let r = drive(&mut z, &data, Z_FINISH, FIRST_BLOCK, None, None);
        if r.code == Z_STREAM_ERROR {
            return Err(zlib_error(it, Some(&z), r.code, "while compressing data"));
        }
        Ok(r.out)
    }

    /// Returns a bytes object containing the uncompressed data.
    ///
    ///   data
    ///     Compressed data.
    ///   wbits
    ///     The window buffer size and container format.
    ///   bufsize
    ///     The initial output buffer size.
    #[op(hint(py(text_signature = "($module, data, /, wbits=MAX_WBITS, bufsize=DEF_BUF_SIZE)")))]
    fn decompress(it: &mut Interp, data: &Value, #[kw] #[default(15)] wbits: i64, #[kw] #[default(16384)] bufsize: i64) -> R<Vec<u8>> {
        let data = buffer(it, data)?;
        let wbits = c_int(it, wbits)?;
        if bufsize < 0 {
            return Err(it.value_error("bufsize must be non-negative"));
        }
        let mut z = ZStream::inflate(wbits).map_err(|code| zlib_error(it, None, code, "while preparing to decompress data"))?;
        let r = drive(&mut z, &data, Z_FINISH, (bufsize as usize).max(1), None, None);
        if r.code != Z_STREAM_END {
            return Err(zlib_error(it, Some(&z), r.code, "while decompressing data"));
        }
        Ok(r.out)
    }

    /// Return a compressor object.
    ///
    ///   level
    ///     The compression level (an integer in the range 0-9 or -1; default is
    ///     currently equivalent to 6).  Higher compression levels are slower,
    ///     but produce smaller results.
    ///   method
    ///     The compression algorithm.  If given, this must be DEFLATED.
    ///   wbits
    ///     +9 to +15: The base-two logarithm of the window size.  Include a zlib
    ///         container.
    ///     -9 to -15: Generate a raw stream.
    ///     +25 to +31: Include a gzip container.
    ///   memLevel
    ///     Controls the amount of memory used for internal compression state.
    ///     Valid values range from 1 to 9.  Higher values result in higher memory
    ///     usage, faster compression, and smaller output.
    ///   strategy
    ///     Used to tune the compression algorithm.  Possible values are
    ///     Z_DEFAULT_STRATEGY, Z_FILTERED, and Z_HUFFMAN_ONLY.
    ///   zdict
    ///     The predefined compression dictionary - a sequence of bytes
    ///     containing subsequences that are likely to occur in the input data.
    #[op(hint(py(text_signature = "($module, /, level=Z_DEFAULT_COMPRESSION, method=DEFLATED,\n            wbits=MAX_WBITS, memLevel=DEF_MEM_LEVEL,\n            strategy=Z_DEFAULT_STRATEGY, zdict=None)")))]
    #[allow(non_snake_case)]
    fn compressobj(
        it: &mut Interp,
        #[kw] #[default(-1)] level: i64,
        #[kw] #[default(8)] method: i64,
        #[kw] #[default(15)] wbits: i64,
        #[kw] #[default(8)] memLevel: i64,
        #[kw] #[default(0)] strategy: i64,
        #[kw] zdict: Option<&Value>,
    ) -> R<Value> {
        let (level, wbits, mem_level, strategy) = (c_int(it, level)?, c_int(it, wbits)?, c_int(it, memLevel)?, c_int(it, strategy)?);
        let zdict = match zdict {
            Some(v) => Some(buffer(it, v)?),
            None => None,
        };
        if method != 8 {
            return Err(it.value_error("Invalid initialization option"));
        }
        let mut z = match ZStream::deflate(level, wbits, mem_level, strategy) {
            Ok(z) => z,
            Err(Z_STREAM_ERROR) => return Err(it.value_error("Invalid initialization option")),
            Err(code) => return Err(zlib_error(it, None, code, "while creating compression object")),
        };
        if let Some(d) = zdict {
            match z.set_dictionary(&d) {
                Z_OK => {}
                Z_STREAM_ERROR => return Err(it.value_error("Invalid dictionary")),
                _ => return Err(it.value_error("deflateSetDictionary()")),
            }
        }
        Ok(Py::new(it, Compress { z: Some(z) }).value().clone())
    }

    /// Return a decompressor object.
    ///
    ///   wbits
    ///     The window buffer size and container format.
    ///   zdict
    ///     The predefined compression dictionary.  This must be the same
    ///     dictionary as used by the compressor that produced the input data.
    #[op(hint(py(text_signature = "($module, /, wbits=MAX_WBITS, zdict=b'')")))]
    fn decompressobj(it: &mut Interp, #[kw] #[default(15)] wbits: i64, #[kw] zdict: Option<&Value>) -> R<Value> {
        let wbits = c_int(it, wbits)?;
        let zdict = zdict_arg(it, zdict)?;
        let z = new_inflate(it, wbits, zdict.as_deref())?;
        let d = Decompress { z: Some(z), zdict, unused_data: Vec::new(), unconsumed_tail: Vec::new(), eof: false };
        Ok(Py::new(it, d).value().clone())
    }

    fn zdict_arg(it: &mut Interp, zdict: Option<&Value>) -> R<Option<Vec<u8>>> {
        match zdict {
            None => Ok(None),
            Some(v) => buffer(it, v).map(Some).map_err(|_| it.type_error("zdict argument must support the buffer protocol")),
        }
    }

    fn new_inflate(it: &mut Interp, wbits: i32, zdict: Option<&[u8]>) -> R<ZStream> {
        let mut z = match ZStream::inflate(wbits) {
            Ok(z) => z,
            Err(Z_STREAM_ERROR) => return Err(it.value_error("Invalid initialization option")),
            Err(code) => return Err(zlib_error(it, None, code, "while creating decompression object")),
        };
        if let (Some(d), true) = (zdict, wbits < 0) {
            let code = z.set_dictionary(d);
            if code != Z_OK {
                return Err(zlib_error(it, Some(&z), code, "while setting zdict"));
            }
        }
        Ok(z)
    }

    /// Compute an Adler-32 checksum of data.
    ///
    ///   value
    ///     Starting value of the checksum.
    ///
    /// The returned checksum is an integer.
    #[op(hint(py(text_signature = "($module, data, value=1, /)")))]
    fn adler32(it: &mut Interp, data: &Value, value: Option<&Value>) -> R<u32> {
        let data = buffer(it, data)?;
        let seed = match value {
            Some(v) => bitwise_u32(it, v)?,
            None => 1,
        };
        Ok(zc::adler32_from(seed, &data))
    }

    /// Compute a CRC-32 checksum of data.
    ///
    ///   value
    ///     Starting value of the checksum.
    ///
    /// The returned checksum is an integer.
    #[op(hint(py(text_signature = "($module, data, value=0, /)")))]
    fn crc32(it: &mut Interp, data: &Value, value: Option<&Value>) -> R<u32> {
        let data = buffer(it, data)?;
        let seed = match value {
            Some(v) => bitwise_u32(it, v)?,
            None => 0,
        };
        Ok(zc::crc32_from(seed, &data))
    }

    #[class(name = "Compress", module = "zlib", hint(py(final)))]
    pub struct Compress {
        /// `None` once `flush(Z_FINISH)` ended the stream.
        z: Option<ZStream>,
    }

    #[methods]
    impl Compress {
        /// Returns a bytes object containing compressed data.
        ///
        ///   data
        ///     Binary data to be compressed.
        ///
        /// After calling this function, some of the input data may still
        /// be stored in internal buffers for later processing.
        /// Call the flush() method to clear these buffers.
        fn compress(slf: This<Py<Self>>, it: &mut Interp, data: &Value) -> R<Vec<u8>> {
            let data = buffer(it, data)?;
            let mut s = slf.0.borrow_mut(it)?;
            let Some(z) = s.z.as_mut() else {
                drop(s);
                return Err(zlib_error(it, None, Z_STREAM_ERROR, "while compressing data"));
            };
            let r = drive(z, &data, Z_NO_FLUSH, FIRST_BLOCK, None, None);
            if r.code == Z_STREAM_ERROR {
                let e = zlib_error(it, Some(z), r.code, "while compressing data");
                return Err(e);
            }
            Ok(r.out)
        }

        /// Return a bytes object containing any remaining compressed data.
        ///
        ///   mode
        ///     One of the constants Z_SYNC_FLUSH, Z_FULL_FLUSH, Z_FINISH.
        ///     If mode == Z_FINISH, the compressor object can no longer be
        ///     used after calling the flush() method.  Otherwise, more data
        ///     can still be compressed.
        #[method(hint(py(text_signature = "($self, mode=zlib.Z_FINISH, /)")))]
        fn flush(slf: This<Py<Self>>, it: &mut Interp, #[default(4)] mode: i64) -> R<Vec<u8>> {
            let mode = c_int(it, mode)?;
            if mode == Z_NO_FLUSH {
                return Ok(Vec::new());
            }
            let mut s = slf.0.borrow_mut(it)?;
            let Some(z) = s.z.as_mut() else {
                drop(s);
                return Err(zlib_error(it, None, Z_STREAM_ERROR, "while flushing"));
            };
            let r = drive(z, &[], mode, FIRST_BLOCK, None, None);
            if r.code == Z_STREAM_END && mode == Z_FINISH {
                s.z = None;
            } else if !matches!(r.code, Z_OK | Z_BUF_ERROR) {
                let e = zlib_error(it, Some(z), r.code, "while flushing");
                return Err(e);
            }
            Ok(r.out)
        }

        /// Return a copy of the compression object.
        fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let copied = slf.0.borrow_mut(it)?.z.as_mut().map(|z| z.try_clone());
            match copied {
                Some(Ok(z)) => Ok(Py::new(it, Compress { z: Some(z) }).value().clone()),
                None | Some(Err(Z_STREAM_ERROR)) => Err(it.value_error("Inconsistent stream state")),
                Some(Err(code)) => Err(zlib_error(it, None, code, "while copying compression object")),
            }
        }

        #[method(name = "__copy__")]
        fn copy_dunder(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            Self::copy(slf, it)
        }

        #[method(name = "__deepcopy__")]
        fn deepcopy(slf: This<Py<Self>>, it: &mut Interp, memo: &Value) -> R<Value> {
            let _ = memo;
            Self::copy(slf, it)
        }
    }

    #[class(name = "Decompress", module = "zlib", hint(py(final)))]
    pub struct Decompress {
        /// `None` once `flush()` reached the end of the stream.
        z: Option<ZStream>,
        zdict: Option<Vec<u8>>,
        unused_data: Vec<u8>,
        unconsumed_tail: Vec<u8>,
        eof: bool,
    }

    impl Decompress {
        /// `save_unconsumed_input`: input after the end of the stream goes to `unused_data`, input
        /// left by the output limit to `unconsumed_tail`.
        fn save_unconsumed(&mut self, data: &[u8], consumed: usize, code: i32) {
            let left = &data[consumed..];
            let mut avail = left.len();
            if code == Z_STREAM_END && avail > 0 {
                self.unused_data.extend_from_slice(left);
                avail = 0;
            }
            if avail > 0 || !self.unconsumed_tail.is_empty() {
                self.unconsumed_tail = left.to_vec();
            }
        }
    }

    #[methods]
    impl Decompress {
        /// Return a bytes object containing the decompressed version of the data.
        ///
        ///   data
        ///     The binary data to decompress.
        ///   max_length
        ///     The maximum allowable length of the decompressed data.
        ///     Unconsumed input data will be stored in
        ///     the unconsumed_tail attribute.
        ///
        /// After calling this function, some of the input data may still be stored in
        /// internal buffers for later processing.
        /// Call the flush() method to clear these buffers.
        fn decompress(slf: This<Py<Self>>, it: &mut Interp, data: &Value, #[kw] #[default(0)] max_length: i64) -> R<Vec<u8>> {
            let data = buffer(it, data)?;
            if max_length < 0 {
                return Err(it.value_error("max_length must be non-negative"));
            }
            let max = (max_length > 0).then_some(max_length as usize);
            let mut guard = slf.0.borrow_mut(it)?;
            let s = &mut *guard;
            let Some(z) = s.z.as_mut() else {
                drop(guard);
                return Err(zlib_error(it, None, Z_STREAM_ERROR, "while decompressing data"));
            };
            let first = max.map_or(FIRST_BLOCK, |m| m.min(FIRST_BLOCK));
            let r = drive(z, &data, Z_SYNC_FLUSH, first, max, s.zdict.as_deref());
            if !matches!(r.code, Z_OK | Z_BUF_ERROR | Z_STREAM_END) {
                let e = zlib_error(it, Some(z), r.code, "while decompressing data");
                s.save_unconsumed(&data, r.consumed, r.code);
                return Err(e);
            }
            s.save_unconsumed(&data, r.consumed, r.code);
            if r.code == Z_STREAM_END {
                s.eof = true;
            }
            Ok(r.out)
        }

        /// Return a bytes object containing any remaining decompressed data.
        ///
        ///   length
        ///     the initial size of the output buffer.
        fn flush(slf: This<Py<Self>>, it: &mut Interp, #[default(16384)] length: i64) -> R<Vec<u8>> {
            if length <= 0 {
                return Err(it.value_error("length must be greater than zero"));
            }
            let mut guard = slf.0.borrow_mut(it)?;
            let s = &mut *guard;
            let Some(z) = s.z.as_mut() else { return Ok(Vec::new()) };
            let tail = s.unconsumed_tail.clone();
            let r = drive(z, &tail, Z_FINISH, length as usize, None, s.zdict.as_deref());
            s.save_unconsumed(&tail, r.consumed, r.code);
            if r.code == Z_STREAM_END {
                s.eof = true;
                s.z = None;
            }
            Ok(r.out)
        }

        /// Return a copy of the decompression object.
        fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let copied = {
                let mut guard = slf.0.borrow_mut(it)?;
                let s = &mut *guard;
                s.z.as_mut().map(|z| {
                    z.try_clone().map(|z| Decompress {
                        z: Some(z),
                        zdict: s.zdict.clone(),
                        unused_data: s.unused_data.clone(),
                        unconsumed_tail: s.unconsumed_tail.clone(),
                        eof: s.eof,
                    })
                })
            };
            match copied {
                Some(Ok(d)) => Ok(Py::new(it, d).value().clone()),
                None | Some(Err(Z_STREAM_ERROR)) => Err(it.value_error("Inconsistent stream state")),
                Some(Err(code)) => Err(zlib_error(it, None, code, "while copying decompression object")),
            }
        }

        #[method(name = "__copy__")]
        fn copy_dunder(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            Self::copy(slf, it)
        }

        #[method(name = "__deepcopy__")]
        fn deepcopy(slf: This<Py<Self>>, it: &mut Interp, memo: &Value) -> R<Value> {
            let _ = memo;
            Self::copy(slf, it)
        }

        #[getter]
        fn unused_data(&self) -> Vec<u8> {
            self.unused_data.clone()
        }

        #[getter]
        fn unconsumed_tail(&self) -> Vec<u8> {
            self.unconsumed_tail.clone()
        }

        #[getter]
        fn eof(&self) -> bool {
            self.eof
        }
    }

    /// Create a decompressor object for decompressing data incrementally.
    ///
    ///   wbits = 15
    ///   zdict
    ///      The predefined compression dictionary. This is a sequence of bytes
    ///      (such as a bytes object) containing subsequences that are expected
    ///      to occur frequently in the data that is to be compressed. Those
    ///      subsequences that are expected to be most common should come at the
    ///      end of the dictionary. This must be the same dictionary as used by the
    ///      compressor that produced the input data.
    #[class(name = "_ZlibDecompressor", module = "zlib", hint(py(final)))]
    pub struct ZlibDecompressor {
        core: Decompressor<Inflate>,
    }

    struct Inflate {
        /// `None` once the end of the stream was reached.
        z: Option<ZStream>,
        zdict: Option<Vec<u8>>,
    }

    impl Codec for Inflate {
        type Error = i32;

        fn run(&mut self, input: &[u8], max: Option<usize>) -> Result<Chunk, i32> {
            let Some(z) = self.z.as_mut() else {
                return Ok(Chunk { out: Vec::new(), consumed: 0, eof: false, room: true });
            };
            let first = max.map_or(DEF_BUF_SIZE, |m| m.min(MAX_INITIAL_BUF));
            let r = drive(z, input, Z_SYNC_FLUSH, first, max, self.zdict.as_deref());
            if !matches!(r.code, Z_OK | Z_BUF_ERROR | Z_STREAM_END) {
                return Err(r.code);
            }
            let eof = r.code == Z_STREAM_END;
            if eof {
                self.z = None;
            }
            Ok(Chunk { out: r.out, consumed: r.consumed, eof, room: true })
        }
    }

    #[methods]
    impl ZlibDecompressor {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[kw] #[default(15)] wbits: i64, #[kw] zdict: Option<&Value>) -> R<Value> {
            let wbits = c_int(it, wbits)?;
            let zdict = match zdict {
                Some(v) => Some(buffer(it, v)?),
                None => None,
            };
            let z = new_inflate(it, wbits, zdict.as_deref())?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, ZlibDecompressor { core: Decompressor::new(Inflate { z: Some(z), zdict }) }))
        }

        /// Decompress *data*, returning uncompressed data as bytes.
        ///
        /// If *max_length* is nonnegative, returns at most *max_length* bytes of
        /// decompressed data. If this limit is reached and further output can be
        /// produced, *self.needs_input* will be set to ``False``. In this case, the next
        /// call to *decompress()* may provide *data* as b'' to obtain more of the output.
        ///
        /// If all of the input data was decompressed and returned (either because this
        /// was less than *max_length* bytes, or because *max_length* was negative),
        /// *self.needs_input* will be set to True.
        ///
        /// Attempting to decompress data after the end of stream is reached raises an
        /// EOFError.  Any data found after the end of the stream is ignored and saved in
        /// the unused_data attribute.
        fn decompress(slf: This<Py<Self>>, it: &mut Interp, #[kw] data: &Value, #[kw] #[default(-1)] max_length: i64) -> R<Vec<u8>> {
            let data = buffer(it, data)?;
            let max = (0..i64::MAX).contains(&max_length).then_some(max_length as usize);
            let mut guard = slf.0.borrow_mut(it)?;
            let s = &mut *guard;
            s.core.decompress(&data, max).map_err(|e| match e {
                Failure::AtEof => it.new_exc_str("EOFError", "End of stream already reached"),
                Failure::Codec(code) => zlib_error(it, s.core.codec.z.as_ref(), code, "while decompressing data"),
            })
        }

        /// True if the end-of-stream marker has been reached.
        #[getter]
        fn eof(&self) -> bool {
            self.core.eof
        }

        /// Data found after the end of the compressed stream.
        #[getter]
        fn unused_data(&self) -> Vec<u8> {
            self.core.unused_data.clone()
        }

        /// True if more input is needed before more decompressed data can be produced.
        #[getter]
        fn needs_input(&self) -> bool {
            self.core.needs_input
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let exc = it.exc_type("Exception");
        let err = crate::builtins::native::new_type(it, "zlib", "error", Some(&exc), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(err.clone()));
        it.native_state::<State>().error = Some(err);
        let ints: [(&str, i64); 20] = [
            ("MAX_WBITS", MAX_WBITS),
            ("DEFLATED", 8),
            ("DEF_MEM_LEVEL", DEF_MEM_LEVEL as i64),
            ("DEF_BUF_SIZE", DEF_BUF_SIZE as i64),
            ("Z_NO_COMPRESSION", 0),
            ("Z_BEST_SPEED", 1),
            ("Z_BEST_COMPRESSION", 9),
            ("Z_DEFAULT_COMPRESSION", -1),
            ("Z_FILTERED", 1),
            ("Z_HUFFMAN_ONLY", 2),
            ("Z_RLE", 3),
            ("Z_FIXED", 4),
            ("Z_DEFAULT_STRATEGY", 0),
            ("Z_NO_FLUSH", Z_NO_FLUSH as i64),
            ("Z_PARTIAL_FLUSH", 1),
            ("Z_SYNC_FLUSH", Z_SYNC_FLUSH as i64),
            ("Z_FULL_FLUSH", 3),
            ("Z_FINISH", Z_FINISH as i64),
            ("Z_BLOCK", 5),
            ("Z_TREES", 6),
        ];
        for (name, v) in ints {
            dict_set_str(&d, name, Value::Int(v));
        }
        let version = zc::zlib_version();
        dict_set_str(&d, "ZLIB_VERSION", Value::string(version.clone()));
        dict_set_str(&d, "ZLIB_RUNTIME_VERSION", Value::string(version));
        dict_set_str(&d, "__version__", Value::str("1.0"));
    }
}
