//! Native ops behind `node:crypto`'s digests/KDFs, `Buffer`'s codecs and search, the built-in
//! `bufferutil` / `utf-8-validate` fallbacks, and the truly asynchronous `fs` read/write — bound
//! with the `#[op]` / `#[class]` macros. The JS glue reaches them as `__native`
//! (`__node.native()` builds the namespace once, in preamble.js).
//!
//! `#[op(async)]` ops run their body on the runtime's worker pool and settle the returned promise
//! on the JS thread when it finishes (see `lumen::embed::AsyncHost`), so a 100k-iteration PBKDF2
//! or an 8 MB file read no longer blocks timers and I/O.

use lumen::embed::{Ctx, SendError, Value};
use lumen_common::search;

use crate::codec;

pub(crate) use bindings::*;

#[lumen_bind::module(name = "native")]
pub(crate) mod bindings {
    use super::*;

    // ---- Buffer -------------------------------------------------------------------------------------

    /// `Buffer.from(string, encoding)`.
    #[op]
    fn encode(s: &str, enc: u32) -> Vec<u8> {
        codec::encode(s, enc)
    }

    /// `buf.toString(encoding, start, end)` over `bytes[start..end]` (clamped).
    #[op]
    fn decode(bytes: &[u8], enc: u32, start: u32, end: u32) -> String {
        let end = (end as usize).min(bytes.len());
        let start = (start as usize).min(end);
        codec::decode(&bytes[start..end], enc)
    }

    #[op(name = "byteLength")]
    fn byte_length(s: &str, enc: u32) -> u32 {
        codec::byte_length(s, enc) as u32
    }

    /// `buf.write(string, offset, length, encoding)`: bytes written (never a partial character).
    #[op]
    fn write(buf: &mut [u8], s: &str, enc: u32, offset: u32, length: u32) -> u32 {
        let start = (offset as usize).min(buf.len());
        let end = start.saturating_add(length as usize).min(buf.len());
        codec::write(&mut buf[start..end], s, enc) as u32
    }

    /// Node's `IndexOfOffset`: where a search starts, or `None` for "no match possible".
    fn search_start(len: usize, offset: f64, needle_len: usize, forward: bool) -> Option<usize> {
        let len = len as i64;
        let off = offset as i64; // JS already truncated/clamped to the int32 range
        let nl = needle_len as i64;
        let r = if off < 0 {
            if off + len >= 0 {
                len + off
            } else if forward || nl == 0 {
                0
            } else {
                return None;
            }
        } else if off + nl <= len {
            off
        } else if nl == 0 {
            len
        } else if forward {
            return None;
        } else {
            len - 1
        };
        Some(r as usize)
    }

    fn search(hay: &[u8], needle: &[u8], offset: f64, forward: bool, ucs2: bool) -> f64 {
        let Some(start) = search_start(hay.len(), offset, needle.len(), forward) else {
            return -1.0;
        };
        if needle.is_empty() {
            return start as f64;
        }
        if hay.is_empty() || needle.len() > hay.len() {
            return -1.0;
        }
        if ucs2 {
            return search_ucs2(hay, needle, start, forward);
        }
        let found = if forward {
            search::find_from(hay, needle, start)
        } else {
            search::rfind_upto(hay, needle, start)
        };
        found.map_or(-1.0, |i| i as f64)
    }

    /// Node's UCS-2 search: both sides are read as 16-bit units, so only even byte offsets match
    /// (and a needle or haystack under one unit never does).
    fn search_ucs2(hay: &[u8], needle: &[u8], start: usize, forward: bool) -> f64 {
        if hay.len() < 2 || needle.len() < 2 {
            return -1.0;
        }
        let units = hay.len() / 2;
        let needle = &needle[..needle.len() / 2 * 2];
        let nu = needle.len() / 2;
        if nu > units {
            return -1.0;
        }
        let hay = &hay[..units * 2];
        let mut at = start / 2 * 2;
        loop {
            let found = if forward {
                search::find_from(hay, needle, at)
            } else {
                search::rfind_upto(hay, needle, at)
            };
            match found {
                Some(i) if i % 2 == 0 => return i as f64,
                Some(i) if forward => at = i + 1,
                None => return -1.0,
                Some(i) => at = i - 1,
            }
        }
    }

    const ENC_UCS2: u32 = 6;

    /// `buf.indexOf(bytes, byteOffset, encoding)` / `lastIndexOf` (`forward = false`).
    #[op(name = "indexOf")]
    fn index_of(hay: &[u8], needle: &[u8], offset: f64, forward: bool, enc: u32) -> f64 {
        search(hay, needle, offset, forward, enc == ENC_UCS2)
    }

    #[op(name = "indexOfStr")]
    fn index_of_str(hay: &[u8], needle: &str, enc: u32, offset: f64, forward: bool) -> f64 {
        let bytes = codec::encode(needle, enc);
        search(hay, &bytes, offset, forward, enc == ENC_UCS2)
    }

    #[op(name = "indexOfByte")]
    fn index_of_byte(hay: &[u8], byte: u32, offset: f64, forward: bool) -> f64 {
        search(hay, &[byte as u8], offset, forward, false)
    }

    /// Lexicographic comparison: -1, 0 or 1.
    #[op]
    fn compare(a: &[u8], b: &[u8]) -> i32 {
        match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    #[op]
    fn equals(a: &[u8], b: &[u8]) -> bool {
        a == b
    }

    fn fill_pattern(dst: &mut [u8], pat: &[u8]) {
        if pat.len() == 1 {
            dst.fill(pat[0]);
            return;
        }
        let n = pat.len().min(dst.len());
        dst[..n].copy_from_slice(&pat[..n]);
        // Double the filled prefix until the whole range is covered.
        let mut filled = n;
        while filled < dst.len() {
            let k = filled.min(dst.len() - filled);
            dst.copy_within(..k, filled);
            filled += k;
        }
    }

    /// `buf.fill(bytes, start, end)`; the pattern must not alias `buf` (JS copies it first).
    /// Returns -1 for an empty pattern (Node's ERR_INVALID_ARG_VALUE).
    #[op]
    fn fill(buf: &mut [u8], pat: &[u8], start: u32, end: u32) -> i32 {
        if pat.is_empty() {
            return -1;
        }
        let end = (end as usize).min(buf.len());
        let start = (start as usize).min(end);
        fill_pattern(&mut buf[start..end], pat);
        0
    }

    #[op(name = "fillStr")]
    fn fill_str(buf: &mut [u8], s: &str, enc: u32, start: u32, end: u32) -> i32 {
        let pat = codec::encode(s, enc);
        if pat.is_empty() {
            return -1;
        }
        let end = (end as usize).min(buf.len());
        let start = (start as usize).min(end);
        fill_pattern(&mut buf[start..end], &pat);
        0
    }

    /// `swap16/32/64` in place (JS checked the length is a multiple of `width`).
    #[op]
    fn swap(buf: &mut [u8], width: u32) {
        match width {
            2 => buf.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
            4 => buf.chunks_exact_mut(4).for_each(|c| c.reverse()),
            _ => buf.chunks_exact_mut(8).for_each(|c| c.reverse()),
        }
    }

    /// Width in bytes of a `readNum`/`writeNum` kind (see `NUM_KINDS` in buffer.js).
    fn num_width(kind: u32) -> usize {
        match kind {
            0 | 1 => 1,
            2..=5 => 2,
            6..=11 => 4,
            _ => 8,
        }
    }

    /// Start of a `width`-byte access at `offset`, when it is an in-bounds integer.
    fn num_at(len: usize, offset: f64, width: usize) -> Option<usize> {
        if offset >= 0.0 && offset.fract() == 0.0 && offset + width as f64 <= len as f64 {
            Some(offset as usize)
        } else {
            None
        }
    }

    /// The fixed-width `buf.read*` accessors: NaN for an invalid offset (JS then builds Node's
    /// error; a float that really is NaN is told apart there).
    #[op(name = "readNum")]
    fn read_num(buf: &[u8], offset: f64, kind: u32) -> f64 {
        let w = num_width(kind);
        let Some(o) = num_at(buf.len(), offset, w) else {
            return f64::NAN;
        };
        let b = &buf[o..o + w];
        let a2 = |b: &[u8]| [b[0], b[1]];
        let a4 = |b: &[u8]| [b[0], b[1], b[2], b[3]];
        let a8 = |b: &[u8]| [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match kind {
            0 => b[0] as f64,
            1 => b[0] as i8 as f64,
            2 => u16::from_le_bytes(a2(b)) as f64,
            3 => u16::from_be_bytes(a2(b)) as f64,
            4 => i16::from_le_bytes(a2(b)) as f64,
            5 => i16::from_be_bytes(a2(b)) as f64,
            6 => u32::from_le_bytes(a4(b)) as f64,
            7 => u32::from_be_bytes(a4(b)) as f64,
            8 => i32::from_le_bytes(a4(b)) as f64,
            9 => i32::from_be_bytes(a4(b)) as f64,
            10 => f32::from_le_bytes(a4(b)) as f64,
            11 => f32::from_be_bytes(a4(b)) as f64,
            12 => f64::from_le_bytes(a8(b)),
            _ => f64::from_be_bytes(a8(b)),
        }
    }

    /// The fixed-width `buf.write*` accessors (JS has range-checked integer values): the offset
    /// after the write, or -1 for an invalid offset.
    #[op(name = "writeNum")]
    fn write_num(buf: &mut [u8], value: f64, offset: f64, kind: u32) -> f64 {
        let w = num_width(kind);
        let Some(o) = num_at(buf.len(), offset, w) else {
            return -1.0;
        };
        // Integer kinds wrap like the typed-array stores Node uses (ToInt32 truncation, NaN -> 0).
        let int = if value.is_finite() {
            value.trunc() as i64
        } else {
            0
        };
        let d = &mut buf[o..o + w];
        match kind {
            0 | 1 => d[0] = int as u8,
            2 | 4 => d.copy_from_slice(&(int as u16).to_le_bytes()),
            3 | 5 => d.copy_from_slice(&(int as u16).to_be_bytes()),
            6 | 8 => d.copy_from_slice(&(int as u32).to_le_bytes()),
            7 | 9 => d.copy_from_slice(&(int as u32).to_be_bytes()),
            10 => d.copy_from_slice(&(value as f32).to_le_bytes()),
            11 => d.copy_from_slice(&(value as f32).to_be_bytes()),
            12 => d.copy_from_slice(&value.to_le_bytes()),
            _ => d.copy_from_slice(&value.to_be_bytes()),
        }
        (o + w) as f64
    }

    #[op(name = "isUtf8")]
    fn is_utf8(bytes: &[u8]) -> bool {
        codec::is_utf8(bytes)
    }

    #[op(name = "isAscii")]
    fn is_ascii(bytes: &[u8]) -> bool {
        bytes.is_ascii()
    }

    // ---- bufferutil / utf-8-validate ------------------------------------------------------------------

    /// XOR `src` into `out[offset..offset+length]` with a repeating 4-byte key (`key` holds the key's
    /// bytes little-endian, so byte `i` of the key is `key >> 8*(i%4)`).
    fn xor_mask(src: &[u8], out: &mut [u8], key: u32) {
        let k8 = u64::from(key) | u64::from(key) << 32;
        let n = src.len().min(out.len());
        let (src, out) = (&src[..n], &mut out[..n]);
        let mut s = src.chunks_exact(8);
        let mut o = out.chunks_exact_mut(8);
        for (a, b) in (&mut s).zip(&mut o) {
            let w = u64::from_le_bytes(a.try_into().unwrap()) ^ k8;
            b.copy_from_slice(&w.to_le_bytes());
        }
        let kb = key.to_le_bytes();
        for (i, (a, b)) in s.remainder().iter().zip(o.into_remainder()).enumerate() {
            *b = a ^ kb[i & 3];
        }
    }

    /// bufferutil's `mask(source, mask, output, offset, length)` (distinct buffers).
    #[op]
    fn mask(src: &[u8], key: u32, out: &mut [u8], offset: u32, length: u32) {
        let start = (offset as usize).min(out.len());
        let end = start.saturating_add(length as usize).min(out.len());
        let n = (end - start).min(src.len());
        xor_mask(&src[..n], &mut out[start..start + n], key);
    }

    /// bufferutil's `unmask(buffer, mask)`, in place.
    #[op]
    fn unmask(buf: &mut [u8], key: u32) {
        let k8 = u64::from(key) | u64::from(key) << 32;
        let mut c = buf.chunks_exact_mut(8);
        for b in &mut c {
            let w = u64::from_le_bytes((&*b).try_into().unwrap()) ^ k8;
            b.copy_from_slice(&w.to_le_bytes());
        }
        let kb = key.to_le_bytes();
        for (i, b) in c.into_remainder().iter_mut().enumerate() {
            *b ^= kb[i & 3];
        }
    }

    /// Modules lumen provides only when `node_modules` does not: the native addons `ws` (and
    /// others) load optionally, so `require('bufferutil')` finds the real package when installed
    /// and this built-in otherwise. The exports are registered in addons.js.
    const FALLBACK_MODULES: &[&str] = &["bufferutil", "utf-8-validate"];

    #[op(name = "isFallbackModule")]
    fn is_fallback_module(name: &str) -> bool {
        FALLBACK_MODULES.contains(&name)
    }

    // ---- fs -----------------------------------------------------------------------------------------

    /// The (code, description) Node prints for an I/O error.
    fn io_code(e: &std::io::Error) -> (&'static str, &'static str) {
        let code = lumen_os::errno::uv_code(e);
        (code, lumen_os::uv::message(code).unwrap_or("unknown error"))
    }

    /// JS rebuilds the error with `syscall` and `path` (fsError) from `code` + message.
    fn io_err(e: std::io::Error) -> SendError {
        let (code, desc) = io_code(&e);
        SendError::new("Error", desc).with_code(code)
    }

    /// `fs.readFile(path)` / `fsPromises.readFile(path)`: read on a worker thread.
    #[op(async, name = "readFile")]
    fn read_file(path: String) -> Result<Vec<u8>, SendError> {
        use std::io::Read;
        let mut f = std::fs::File::open(&path).map_err(io_err)?;
        if f.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            return Err(
                SendError::new("Error", "illegal operation on a directory").with_code("EISDIR")
            );
        }
        let mut out = Vec::with_capacity(f.metadata().map(|m| m.len() as usize + 1).unwrap_or(0));
        f.read_to_end(&mut out).map_err(io_err)?;
        Ok(out)
    }

    /// `fs.writeFile` / `fs.appendFile` and their promise forms: `mode` is flagToMode's
    /// `r+`/`w`/`w+`/`a`/`a+` with an optional trailing `x` (exclusive create).
    #[op(async, name = "writeFile")]
    fn write_file(
        path: String,
        data: Vec<u8>,
        mode: String,
        perm: Option<u32>,
    ) -> Result<(), SendError> {
        use std::io::Write;
        let (base, exclusive) = match mode.strip_suffix('x') {
            Some(b) => (b.to_string(), true),
            None => (mode.clone(), false),
        };
        let mut o = std::fs::OpenOptions::new();
        match base.as_str() {
            "a" | "a+" => {
                o.append(true).create(true);
                if base == "a+" {
                    o.read(true);
                }
            }
            "r+" => {
                o.read(true).write(true);
            }
            "w+" => {
                o.read(true).write(true).create(true).truncate(true);
            }
            _ => {
                o.write(true).create(true).truncate(true);
            }
        }
        if exclusive {
            o.create_new(true);
        }
        #[cfg(unix)]
        if let Some(p) = perm {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(p);
        }
        #[cfg(not(unix))]
        let _ = perm;
        let mut f = o.open(&path).map_err(io_err)?;
        f.write_all(&data).map_err(io_err)?;
        Ok(())
    }

    // ---- registration -------------------------------------------------------------------------------

    /// `__node.native()`: a fresh object holding every op above (called once by preamble.js).
    pub fn op_native(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        ctx.module_object::<Module>()
    }
}
