//! bzip2 over the pure-Rust `bzip2` / `libbz2-rs-sys` port of libbzip2, in the same streaming shape
//! as the zlib wrapper: explicit input / output windows and the library's own status codes, so
//! Python's `_bz2` can reproduce libbzip2's write semantics exactly. Pure Rust, so it also builds
//! for wasm.

use bzip2::{Action, Compress, Compression, Decompress, Error, Status};

/// libbzip2's non-error outcomes that matter to callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bz2Status {
    /// More input or output room is needed (`BZ_OK`, `BZ_RUN_OK`, `BZ_FINISH_OK`, `BZ_FLUSH_OK`).
    Ok,
    /// The end-of-stream marker was processed (`BZ_STREAM_END`).
    StreamEnd,
}

/// libbzip2's error codes, as `BZ2_bzCompress` / `BZ2_bzDecompress` return them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bz2Error {
    /// `BZ_SEQUENCE_ERROR`
    Sequence,
    /// `BZ_DATA_ERROR` and `BZ_DATA_ERROR_MAGIC`
    Data,
    /// `BZ_PARAM_ERROR`
    Param,
    /// `BZ_MEM_ERROR`
    Mem,
}

/// What one call did: the outcome plus how much of the input window was consumed and how much of
/// the output window was filled.
#[derive(Clone, Copy, Debug)]
pub struct Bz2Step {
    pub status: Result<Bz2Status, Bz2Error>,
    pub consumed: usize,
    pub produced: usize,
}

fn error(e: Error) -> Bz2Error {
    match e {
        Error::Sequence => Bz2Error::Sequence,
        Error::Data | Error::DataMagic => Bz2Error::Data,
        Error::Param => Bz2Error::Param,
    }
}

fn status(s: Status) -> Bz2Status {
    match s {
        Status::StreamEnd => Bz2Status::StreamEnd,
        Status::Ok | Status::FlushOk | Status::RunOk | Status::FinishOk | Status::MemNeeded => Bz2Status::Ok,
    }
}

/// A `bz_stream` compressing.
pub struct Bz2Encoder {
    inner: Compress,
}

impl Bz2Encoder {
    /// `BZ2_bzCompressInit` with block size `level` (1-9) and the default work factor.
    pub fn new(level: u32) -> Self {
        Bz2Encoder { inner: Compress::new(Compression::new(level.clamp(1, 9)), 0) }
    }

    /// `BZ2_bzCompress` with `BZ_RUN` (`finish == false`) or `BZ_FINISH`.
    pub fn run(&mut self, finish: bool, input: &[u8], output: &mut [u8]) -> Bz2Step {
        let (in_before, out_before) = (self.inner.total_in(), self.inner.total_out());
        let action = if finish { Action::Finish } else { Action::Run };
        let result = self.inner.compress(input, output, action);
        Bz2Step {
            status: result.map(status).map_err(error),
            consumed: (self.inner.total_in() - in_before) as usize,
            produced: (self.inner.total_out() - out_before) as usize,
        }
    }
}

/// A `bz_stream` decompressing.
pub struct Bz2Decoder {
    inner: Decompress,
}

impl Bz2Decoder {
    pub fn new() -> Self {
        Bz2Decoder { inner: Decompress::new(false) }
    }

    /// `BZ2_bzDecompress`.
    pub fn run(&mut self, input: &[u8], output: &mut [u8]) -> Bz2Step {
        let (in_before, out_before) = (self.inner.total_in(), self.inner.total_out());
        let result = self.inner.decompress(input, output);
        let status = match result {
            Ok(Status::MemNeeded) => Err(Bz2Error::Mem),
            other => other.map(status).map_err(error),
        };
        Bz2Step {
            status,
            consumed: (self.inner.total_in() - in_before) as usize,
            produced: (self.inner.total_out() - out_before) as usize,
        }
    }
}

impl Default for Bz2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot compression of `data` at block size `level` (1-9).
pub fn bz2_compress(data: &[u8], level: u32) -> Vec<u8> {
    let mut enc = Bz2Encoder::new(level);
    let mut out = vec![0u8; data.len() / 2 + 1024];
    let (mut consumed, mut produced) = (0, 0);
    loop {
        let step = enc.run(true, &data[consumed..], &mut out[produced..]);
        consumed += step.consumed;
        produced += step.produced;
        if matches!(step.status, Ok(Bz2Status::StreamEnd)) || step.status.is_err() {
            out.truncate(produced);
            return out;
        }
        if produced == out.len() {
            out.resize(out.len() * 2, 0);
        }
    }
}

/// One-shot decompression of a single bzip2 stream; trailing bytes after it are an error.
pub fn bz2_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = Bz2Decoder::new();
    let mut out = vec![0u8; data.len().saturating_mul(4).max(1024)];
    let (mut consumed, mut produced) = (0, 0);
    loop {
        let step = dec.run(&data[consumed..], &mut out[produced..]);
        consumed += step.consumed;
        produced += step.produced;
        match step.status {
            Ok(Bz2Status::StreamEnd) => {
                if consumed < data.len() {
                    return Err("bzip2: trailing data after the end of the stream".into());
                }
                out.truncate(produced);
                return Ok(out);
            }
            Ok(Bz2Status::Ok) => {
                if produced == out.len() {
                    out.resize(out.len() * 2, 0);
                } else if consumed == data.len() {
                    return Err("bzip2: unexpected end of input".into());
                }
            }
            Err(_) => return Err("bzip2: invalid data".into()),
        }
    }
}
