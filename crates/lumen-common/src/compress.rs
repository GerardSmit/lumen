//! Compression codecs for the runtime: safe wrappers over maintained crates (zlib-rs through
//! `libz-rs-sys`, `brotli`, `zstd`; `bz2` over `bzip2`, `xz` in pure Rust). The wrappers keep the streaming shape of the C libraries
//! (explicit input/output windows, flush modes, status codes) so `node:zlib` can reproduce
//! zlib's and Brotli's exact write semantics on top of them; the one-shot helpers serve
//! `CompressionStream`, `Bun.zstd*` and friends.

#[cfg(feature = "compress")]
pub mod bz2;
#[cfg(feature = "compress")]
mod modern;
#[cfg(feature = "lzma")]
pub mod xz;

#[cfg(feature = "compress")]
pub use modern::*;

use std::ffi::c_int;

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
    /// Output `deflateParams` flushed while no output window was open; the next `run` emits it.
    carry: Vec<u8>,
}

fn stream_size() -> c_int {
    std::mem::size_of::<z::z_stream>() as c_int
}

impl ZStream {
    /// `deflateInit2`; `window_bits` carries zlib's wrapper convention (negative: raw, +16: gzip).
    pub fn deflate(
        level: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: i32,
    ) -> Result<Self, i32> {
        let mut strm = Box::new(z::z_stream::default());
        // SAFETY: `strm` is a live, default-initialised stream with its allocator set.
        let code = unsafe {
            z::deflateInit2_(
                &mut *strm,
                level,
                8,
                window_bits,
                mem_level,
                strategy,
                z::zlibVersion(),
                stream_size(),
            )
        };
        if code != Z_OK {
            return Err(code);
        }
        Ok(ZStream {
            strm,
            deflating: true,
            carry: Vec::new(),
        })
    }

    /// `inflateInit2`; `window_bits` as for [`ZStream::deflate`] (+32: auto-detect zlib or gzip).
    pub fn inflate(window_bits: i32) -> Result<Self, i32> {
        let mut strm = Box::new(z::z_stream::default());
        // SAFETY: as for `deflate`.
        let code =
            unsafe { z::inflateInit2_(&mut *strm, window_bits, z::zlibVersion(), stream_size()) };
        if code != Z_OK {
            return Err(code);
        }
        Ok(ZStream {
            strm,
            deflating: false,
            carry: Vec::new(),
        })
    }

    pub fn run(&mut self, flush: i32, input: &[u8], output: &mut [u8]) -> ZStep {
        let mut carried = 0;
        if !self.carry.is_empty() {
            carried = self.carry.len().min(output.len());
            output[..carried].copy_from_slice(&self.carry[..carried]);
            self.carry.drain(..carried);
            if !self.carry.is_empty() {
                return ZStep {
                    code: Z_OK,
                    consumed: 0,
                    produced: carried,
                };
            }
        }
        let output = &mut output[carried..];
        let mut step = self.run_window(flush, input, output);
        step.produced += carried;
        step
    }

    fn run_window(&mut self, flush: i32, input: &[u8], output: &mut [u8]) -> ZStep {
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
                z::deflateSetDictionary(
                    &mut *self.strm,
                    dictionary.as_ptr(),
                    dictionary.len() as u32,
                )
            } else {
                z::inflateSetDictionary(
                    &mut *self.strm,
                    dictionary.as_ptr(),
                    dictionary.len() as u32,
                )
            }
        }
    }

    pub fn params(&mut self, level: i32, strategy: i32) -> i32 {
        let mut scratch = [0u8; 4096];
        let s = &mut *self.strm;
        s.next_in = std::ptr::null();
        s.avail_in = 0;
        s.next_out = scratch.as_mut_ptr();
        s.avail_out = scratch.len() as u32;
        // SAFETY: the stream is initialised; the input window is empty and the output window is the
        // live scratch buffer.
        let code = unsafe { z::deflateParams(s, level, strategy) };
        let produced = scratch.len() - s.avail_out as usize;
        s.next_out = std::ptr::null_mut();
        s.avail_out = 0;
        self.carry.extend_from_slice(&scratch[..produced]);
        code
    }

    pub fn reset(&mut self) -> i32 {
        self.carry.clear();
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

    /// `deflateCopy` / `inflateCopy`: an independent stream in the same state.
    pub fn try_clone(&mut self) -> Result<Self, i32> {
        let mut strm = Box::new(z::z_stream::default());
        // zlib-rs refuses to copy an inflate stream without an output window; lend an empty one.
        let mut empty = [0u8; 1];
        self.strm.next_out = empty.as_mut_ptr();
        self.strm.avail_out = 0;
        let src: *mut z::z_stream = &mut *self.strm;
        // SAFETY: both streams are live; the copy gets its own state pointing back at `strm`.
        let code = unsafe {
            if self.deflating {
                z::deflateCopy(&mut *strm, src)
            } else {
                z::inflateCopy(&mut *strm, src)
            }
        };
        self.strm.next_out = std::ptr::null_mut();
        strm.next_out = std::ptr::null_mut();
        if code != Z_OK {
            return Err(code);
        }
        Ok(ZStream {
            strm,
            deflating: self.deflating,
            carry: self.carry.clone(),
        })
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

/// The zlib version string the library reports.
pub fn zlib_version() -> String {
    // SAFETY: `zlibVersion` returns a NUL-terminated static string.
    unsafe { std::ffi::CStr::from_ptr(z::zlibVersion()) }
        .to_string_lossy()
        .into_owned()
}

#[inline]
pub fn crc32_from(seed: u32, data: &[u8]) -> u32 {
    crate::crc32::crc32_from(seed, data)
}

/// Adler-32 of `data`, continued from `seed` (1 for a fresh checksum).
#[inline]
pub fn adler32_from(seed: u32, data: &[u8]) -> u32 {
    // SAFETY: the pointer and length describe a live slice.
    unsafe { z::adler32_z(seed as _, data.as_ptr(), data.len()) as u32 }
}

fn compress_with(level: i32, window_bits: i32, data: &[u8]) -> Vec<u8> {
    let mut z = ZStream::deflate(level, window_bits, 8, 0).expect("deflate init");
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

fn decompress_with(
    window_bits: i32,
    data: &[u8],
    what: &str,
    limit: usize,
) -> Result<Vec<u8>, String> {
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
    compress_with(-1, -15, data)
}
/// [`deflate`] at zlib's best compression level, for data compressed once and decoded often.
pub fn deflate_best(data: &[u8]) -> Vec<u8> {
    compress_with(9, -15, data)
}
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    compress_with(-1, 15, data)
}
pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    compress_with(-1, 31, data)
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
            (
                zlib_compress(&data),
                zlib_decompress_limited as fn(&[u8], usize) -> _,
            ),
            (gzip_compress(&data), gzip_decompress_limited),
            (deflate(&data), inflate_limited),
            #[cfg(feature = "compress")]
            (zstd_compress(&data), zstd_decompress_limited),
        ] {
            assert_eq!(f(&packed, 99_999).err(), err);
            assert_eq!(f(&packed, 100_000).unwrap(), data);
        }
    }

    #[cfg(feature = "compress")]
    #[test]
    fn zstd_round_trips() {
        let data = b"hello hello hello hello".to_vec();
        assert_eq!(zstd_decompress(&zstd_compress(&data)).unwrap(), data);
    }

    #[cfg(feature = "compress")]
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
