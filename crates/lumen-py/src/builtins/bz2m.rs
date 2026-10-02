//! `_bz2` on the shared bzip2 stream in `lumen_common::compress::bz2`, with CPython's buffer,
//! flush and error semantics (`Modules/_bz2module.c`).

/// Implementation module for bz2.
#[lumen_bind::module(name = "_bz2")]
pub mod _bz2 {
    #![allow(clippy::new_ret_no_self)]

    use crate::bind::{opaque_instance, Py, This};
    use crate::object::*;
    use crate::vm::Interp;
    use lumen_common::compress::bz2::{Bz2Decoder, Bz2Encoder, Bz2Error, Bz2Status, Bz2Step};

    const INITIAL_BLOCK: usize = 8 * 1024;

    /// `catch_bz2_error`.
    fn bz2_error(it: &mut Interp, e: Bz2Error) -> Obj {
        match e {
            Bz2Error::Sequence => it.new_exc_str("RuntimeError", "Internal error - Invalid sequence of commands sent to libbzip2"),
            Bz2Error::Data => it.new_exc_str("OSError", "Invalid data stream"),
            Bz2Error::Param => it.new_exc_str("SystemError", "Internal error - invalid parameters passed to libbzip2"),
            Bz2Error::Mem => it.new_exc_str("MemoryError", ""),
        }
    }

    /// Grows `out` when its window is exhausted; `false` once `limit` forbids more room.
    fn make_room(out: &mut Vec<u8>, produced: usize, limit: Option<usize>) -> bool {
        if produced < out.len() {
            return true;
        }
        let grow = out.len().max(INITIAL_BLOCK);
        let len = match limit {
            Some(m) if produced >= m => return false,
            Some(m) => (out.len() + grow).min(m),
            None => out.len() + grow,
        };
        out.resize(len, 0);
        true
    }

    /// Implements `compress` and `flush`: runs the encoder until `finish` reaches the end of the
    /// stream, or (for `BZ_RUN`) until all input is taken.
    fn encode(enc: &mut Bz2Encoder, finish: bool, input: &[u8]) -> Result<Vec<u8>, Bz2Error> {
        let mut out: Vec<u8> = Vec::new();
        let (mut consumed, mut produced) = (0, 0);
        loop {
            make_room(&mut out, produced, None);
            let Bz2Step { status, consumed: c, produced: p } = enc.run(finish, &input[consumed..], &mut out[produced..]);
            consumed += c;
            produced += p;
            match status {
                Err(e) => return Err(e),
                Ok(Bz2Status::StreamEnd) => break,
                Ok(Bz2Status::Ok) => {
                    if !finish && consumed == input.len() {
                        break;
                    }
                }
            }
        }
        out.truncate(produced);
        Ok(out)
    }

    /// Create a compressor object for compressing data incrementally.
    ///
    ///   compresslevel
    ///     Compression level, as a number between 1 and 9.
    ///
    /// For one-shot compression, use the compress() function instead.
    #[class(name = "BZ2Compressor", module = "_bz2", hint(py(final)))]
    pub struct BZ2Compressor {
        enc: Bz2Encoder,
        flushed: bool,
    }

    #[methods]
    impl BZ2Compressor {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[default(9)] compresslevel: i64) -> R<Value> {
            if !(1..=9).contains(&compresslevel) {
                return Err(it.value_error("compresslevel must be between 1 and 9"));
            }
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, BZ2Compressor { enc: Bz2Encoder::new(compresslevel as u32), flushed: false }))
        }

        /// Provide data to the compressor object.
        ///
        /// Returns a chunk of compressed data if possible, or b'' otherwise.
        ///
        /// When you have finished providing data to the compressor, call the
        /// flush() method to finish the compression process.
        fn compress(slf: This<Py<Self>>, it: &mut Interp, data: &Value) -> R<Vec<u8>> {
            let data = it.buffer_bytes(data)?;
            let mut s = slf.0.borrow_mut(it)?;
            if s.flushed {
                drop(s);
                return Err(it.value_error("Compressor has been flushed"));
            }
            match encode(&mut s.enc, false, &data) {
                Ok(out) => Ok(out),
                Err(e) => {
                    drop(s);
                    Err(bz2_error(it, e))
                }
            }
        }

        /// Finish the compression process.
        ///
        /// Returns the compressed data left in internal buffers.
        ///
        /// The compressor object may not be used after this method is called.
        fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<Vec<u8>> {
            let mut s = slf.0.borrow_mut(it)?;
            if s.flushed {
                drop(s);
                return Err(it.value_error("Repeated call to flush()"));
            }
            match encode(&mut s.enc, true, &[]) {
                Ok(out) => {
                    s.flushed = true;
                    Ok(out)
                }
                Err(e) => {
                    drop(s);
                    Err(bz2_error(it, e))
                }
            }
        }
    }

    /// Create a decompressor object for decompressing data incrementally.
    ///
    /// For one-shot decompression, use the decompress() function instead.
    #[class(name = "BZ2Decompressor", module = "_bz2", hint(py(final)))]
    pub struct BZ2Decompressor {
        dec: Bz2Decoder,
        /// Input not yet consumed by libbzip2.
        pending: Vec<u8>,
        unused_data: Vec<u8>,
        eof: bool,
        needs_input: bool,
    }

    impl BZ2Decompressor {
        /// `decompress_buf`: at most `max` bytes out; `pending` is advanced past what was consumed.
        fn drive(&mut self, max: Option<usize>) -> Result<Vec<u8>, Bz2Error> {
            let mut out: Vec<u8> = Vec::new();
            let mut produced = 0;
            let first = max.map_or(INITIAL_BLOCK, |m| m.min(INITIAL_BLOCK));
            out.resize(first, 0);
            let mut consumed = 0;
            let result = loop {
                let step = self.dec.run(&self.pending[consumed..], &mut out[produced..]);
                consumed += step.consumed;
                produced += step.produced;
                match step.status {
                    Err(e) => break Err(e),
                    Ok(Bz2Status::StreamEnd) => {
                        self.eof = true;
                        break Ok(());
                    }
                    Ok(Bz2Status::Ok) => {
                        if consumed == self.pending.len() {
                            break Ok(());
                        }
                        if produced == out.len() && !make_room(&mut out, produced, max) {
                            break Ok(());
                        }
                    }
                }
            };
            self.pending.drain(..consumed);
            result?;
            out.truncate(produced);
            Ok(out)
        }
    }

    #[methods]
    impl BZ2Decompressor {
        #[constructor]
        fn new(cls: This<Value>, _it: &mut Interp) -> R<Value> {
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            let d = BZ2Decompressor {
                dec: Bz2Decoder::new(),
                pending: Vec::new(),
                unused_data: Vec::new(),
                eof: false,
                needs_input: true,
            };
            Ok(opaque_instance(cls, d))
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
            let data = it.buffer_bytes(data)?;
            let mut guard = slf.0.borrow_mut(it)?;
            let s = &mut *guard;
            if s.eof {
                drop(guard);
                return Err(it.new_exc_str("EOFError", "End of stream already reached"));
            }
            s.pending.extend_from_slice(&data);
            let max = (max_length >= 0).then_some(max_length as usize);
            match s.drive(max) {
                Err(e) => {
                    s.needs_input = false;
                    s.pending.clear();
                    drop(guard);
                    Err(bz2_error(it, e))
                }
                Ok(out) => {
                    if s.eof {
                        s.needs_input = false;
                        if !s.pending.is_empty() {
                            s.unused_data = std::mem::take(&mut s.pending);
                        }
                    } else {
                        s.needs_input = s.pending.is_empty();
                    }
                    Ok(out)
                }
            }
        }

        /// True if the end-of-stream marker has been reached.
        #[getter]
        fn eof(&self) -> bool {
            self.eof
        }

        /// Data found after the end of the compressed stream.
        #[getter]
        fn unused_data(&self) -> Vec<u8> {
            self.unused_data.clone()
        }

        /// True if more input is needed before more decompressed data can be produced.
        #[getter]
        fn needs_input(&self) -> bool {
            self.needs_input
        }
    }
}
