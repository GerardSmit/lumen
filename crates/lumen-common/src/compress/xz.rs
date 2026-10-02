//! xz / lzma over liblzma (`liblzma-sys`): the `.xz`, legacy `.lzma` ("alone") and raw container
//! formats, LZMA1 / LZMA2 with the delta and BCJ filters, integrity checks, presets and memory
//! limits. The wrapper keeps liblzma's streaming shape (explicit input / output windows, the
//! library's status codes), which is what Python's `_lzma` is specified against.
//!
//! liblzma is C: there is no pure-Rust codec with the raw / alone / xz stream API, filter chains
//! and check handling this contract needs. The module is therefore behind the `lzma` feature and
//! unavailable on wasm32.

use std::ffi::c_void;
use std::ptr;

use liblzma_sys as l;

/// `LZMA_VLI_UNKNOWN`: the terminator of a filter chain.
pub const VLI_UNKNOWN: u64 = u64::MAX;

pub const FILTER_LZMA1: u64 = 0x4000_0000_0000_0001;
pub const FILTER_LZMA2: u64 = 0x21;
pub const FILTER_DELTA: u64 = 0x03;
pub const FILTER_X86: u64 = 0x04;
pub const FILTER_POWERPC: u64 = 0x05;
pub const FILTER_IA64: u64 = 0x06;
pub const FILTER_ARM: u64 = 0x07;
pub const FILTER_ARMTHUMB: u64 = 0x08;
pub const FILTER_SPARC: u64 = 0x09;

pub const CHECK_NONE: u32 = 0;
pub const CHECK_CRC32: u32 = 1;
pub const CHECK_CRC64: u32 = 4;
pub const CHECK_SHA256: u32 = 10;
pub const CHECK_ID_MAX: u32 = 15;

pub const MF_HC3: u32 = 0x03;
pub const MF_HC4: u32 = 0x04;
pub const MF_BT2: u32 = 0x12;
pub const MF_BT3: u32 = 0x13;
pub const MF_BT4: u32 = 0x14;
pub const MODE_FAST: u32 = 1;
pub const MODE_NORMAL: u32 = 2;
pub const PRESET_DEFAULT: u32 = 6;
pub const PRESET_EXTREME: u32 = 1 << 31;
pub const FILTERS_MAX: usize = 4;

/// Decoder flags (`LZMA_TELL_ANY_CHECK | LZMA_TELL_NO_CHECK`) Python's decompressor uses.
pub const TELL_CHECKS: u32 = l::LZMA_TELL_ANY_CHECK | l::LZMA_TELL_NO_CHECK;

/// liblzma's error codes (`lzma_ret` other than the success family).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XzError {
    UnsupportedCheck,
    Mem,
    MemLimit,
    Format,
    Options,
    Data,
    Buf,
    Prog,
    Other(i32),
}

impl XzError {
    /// The message Python's `LZMAError` carries; `None` for `Mem` (a `MemoryError`).
    pub fn message(self) -> Option<String> {
        Some(
            match self {
                XzError::UnsupportedCheck => "Unsupported integrity check",
                XzError::Mem => return None,
                XzError::MemLimit => "Memory usage limit exceeded",
                XzError::Format => "Input format not supported by decoder",
                XzError::Options => "Invalid or unsupported options",
                XzError::Data => "Corrupt input data",
                XzError::Buf => "Insufficient buffer space",
                XzError::Prog => "Internal error",
                XzError::Other(n) => return Some(format!("Unrecognized error from liblzma: {n}")),
            }
            .to_string(),
        )
    }
}

/// liblzma's success family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XzStatus {
    Ok,
    StreamEnd,
    /// `LZMA_GET_CHECK`: the stream's check type is now known.
    GetCheck,
    /// `LZMA_NO_CHECK`: the stream has no integrity check.
    NoCheck,
}

fn ret(code: l::lzma_ret) -> Result<XzStatus, XzError> {
    match code {
        l::LZMA_OK => Ok(XzStatus::Ok),
        l::LZMA_STREAM_END => Ok(XzStatus::StreamEnd),
        l::LZMA_GET_CHECK => Ok(XzStatus::GetCheck),
        l::LZMA_NO_CHECK => Ok(XzStatus::NoCheck),
        l::LZMA_UNSUPPORTED_CHECK => Err(XzError::UnsupportedCheck),
        l::LZMA_MEM_ERROR => Err(XzError::Mem),
        l::LZMA_MEMLIMIT_ERROR => Err(XzError::MemLimit),
        l::LZMA_FORMAT_ERROR => Err(XzError::Format),
        l::LZMA_OPTIONS_ERROR => Err(XzError::Options),
        l::LZMA_DATA_ERROR => Err(XzError::Data),
        l::LZMA_BUF_ERROR => Err(XzError::Buf),
        l::LZMA_PROG_ERROR => Err(XzError::Prog),
        other => Err(XzError::Other(other as i32)),
    }
}

/// `lzma_options_lzma` without the preset dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LzmaOptions {
    pub dict_size: u32,
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
    pub mode: u32,
    pub nice_len: u32,
    pub mf: u32,
    pub depth: u32,
}

impl LzmaOptions {
    /// `lzma_lzma_preset`: `None` when the preset is invalid.
    pub fn preset(preset: u32) -> Option<LzmaOptions> {
        // SAFETY: `lzma_options_lzma` is plain data (integers and a null-able pointer).
        let mut c: l::lzma_options_lzma = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a live, zeroed options struct.
        if unsafe { l::lzma_lzma_preset(&mut c, preset) } != 0 {
            return None;
        }
        Some(LzmaOptions {
            dict_size: c.dict_size,
            lc: c.lc,
            lp: c.lp,
            pb: c.pb,
            mode: c.mode as u32,
            nice_len: c.nice_len,
            mf: c.mf as u32,
            depth: c.depth,
        })
    }

    fn to_c(self) -> l::lzma_options_lzma {
        // SAFETY: as in `preset`.
        let mut c: l::lzma_options_lzma = unsafe { std::mem::zeroed() };
        c.dict_size = self.dict_size;
        c.preset_dict = ptr::null();
        c.preset_dict_size = 0;
        c.lc = self.lc;
        c.lp = self.lp;
        c.pb = self.pb;
        c.mode = self.mode as _;
        c.nice_len = self.nice_len;
        c.mf = self.mf as _;
        c.depth = self.depth;
        c
    }
}

/// The options of one filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOptions {
    /// LZMA1 and LZMA2.
    Lzma(LzmaOptions),
    Delta { dist: u32 },
    /// A BCJ filter; `None` when decoded from properties that carry no start offset.
    Bcj { start_offset: Option<u32> },
}

/// One entry of a filter chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    pub id: u64,
    pub options: FilterOptions,
}

/// A `lzma_filter` array (terminated by `LZMA_VLI_UNKNOWN`) with its option structs.
struct Chain {
    filters: Vec<l::lzma_filter>,
    _lzma: Vec<Box<l::lzma_options_lzma>>,
    _delta: Vec<Box<l::lzma_options_delta>>,
    _bcj: Vec<Box<l::lzma_options_bcj>>,
}

impl Chain {
    fn new(chain: &[Filter]) -> Chain {
        let mut c = Chain { filters: Vec::with_capacity(chain.len() + 1), _lzma: Vec::new(), _delta: Vec::new(), _bcj: Vec::new() };
        for f in chain {
            let options: *mut c_void = match f.options {
                FilterOptions::Lzma(o) => {
                    let mut b = Box::new(o.to_c());
                    let p = &mut *b as *mut l::lzma_options_lzma as *mut c_void;
                    c._lzma.push(b);
                    p
                }
                FilterOptions::Delta { dist } => {
                    // SAFETY: plain data.
                    let mut o: l::lzma_options_delta = unsafe { std::mem::zeroed() };
                    o.type_ = l::lzma_delta_type_LZMA_DELTA_TYPE_BYTE;
                    o.dist = dist;
                    let mut b = Box::new(o);
                    let p = &mut *b as *mut l::lzma_options_delta as *mut c_void;
                    c._delta.push(b);
                    p
                }
                FilterOptions::Bcj { start_offset } => {
                    let mut b = Box::new(l::lzma_options_bcj { start_offset: start_offset.unwrap_or(0) });
                    let p = &mut *b as *mut l::lzma_options_bcj as *mut c_void;
                    c._bcj.push(b);
                    p
                }
            };
            c.filters.push(l::lzma_filter { id: f.id, options });
        }
        c.filters.push(l::lzma_filter { id: VLI_UNKNOWN, options: ptr::null_mut() });
        c
    }

    fn as_ptr(&self) -> *const l::lzma_filter {
        self.filters.as_ptr()
    }
}

/// What one call to [`XzStream::code`] did.
#[derive(Clone, Copy, Debug)]
pub struct XzStep {
    pub status: Result<XzStatus, XzError>,
    pub consumed: usize,
    pub produced: usize,
}

/// An `lzma_stream`, as an encoder or a decoder depending on the constructor.
pub struct XzStream {
    strm: Box<l::lzma_stream>,
}

// SAFETY: an `lzma_stream` is owned data with no thread affinity; every use goes through `&mut`.
unsafe impl Send for XzStream {}

impl XzStream {
    fn blank() -> XzStream {
        // SAFETY: `LZMA_STREAM_INIT` is the all-zero value.
        XzStream { strm: Box::new(unsafe { std::mem::zeroed() }) }
    }

    fn init(code: impl FnOnce(&mut l::lzma_stream) -> l::lzma_ret) -> Result<XzStream, XzError> {
        let mut s = XzStream::blank();
        ret(code(&mut s.strm))?;
        Ok(s)
    }

    /// `lzma_easy_encoder`: an `.xz` encoder at `preset` with `check`.
    pub fn easy_encoder(preset: u32, check: u32) -> Result<XzStream, XzError> {
        // SAFETY: the stream is a live zeroed `lzma_stream`.
        XzStream::init(|s| unsafe { l::lzma_easy_encoder(s, preset, check as _) })
    }

    /// `lzma_stream_encoder`: an `.xz` encoder with a custom filter chain.
    pub fn stream_encoder(chain: &[Filter], check: u32) -> Result<XzStream, XzError> {
        let chain = Chain::new(chain);
        // SAFETY: the filter array is terminated and lives through the call (liblzma copies it).
        XzStream::init(|s| unsafe { l::lzma_stream_encoder(s, chain.as_ptr(), check as _) })
    }

    /// `lzma_alone_encoder`: a legacy `.lzma` encoder.
    pub fn alone_encoder(options: &LzmaOptions) -> Result<XzStream, XzError> {
        let c = options.to_c();
        // SAFETY: `c` is live through the call.
        XzStream::init(|s| unsafe { l::lzma_alone_encoder(s, &c) })
    }

    /// `lzma_raw_encoder`: a headerless encoder with a filter chain.
    pub fn raw_encoder(chain: &[Filter]) -> Result<XzStream, XzError> {
        let chain = Chain::new(chain);
        // SAFETY: as for `stream_encoder`.
        XzStream::init(|s| unsafe { l::lzma_raw_encoder(s, chain.as_ptr()) })
    }

    /// `lzma_auto_decoder`: `.xz` or `.lzma`, detected from the header.
    pub fn auto_decoder(memlimit: u64, flags: u32) -> Result<XzStream, XzError> {
        // SAFETY: as above.
        XzStream::init(|s| unsafe { l::lzma_auto_decoder(s, memlimit, flags) })
    }

    /// `lzma_stream_decoder`: `.xz` only.
    pub fn stream_decoder(memlimit: u64, flags: u32) -> Result<XzStream, XzError> {
        // SAFETY: as above.
        XzStream::init(|s| unsafe { l::lzma_stream_decoder(s, memlimit, flags) })
    }

    /// `lzma_alone_decoder`: `.lzma` only.
    pub fn alone_decoder(memlimit: u64) -> Result<XzStream, XzError> {
        // SAFETY: as above.
        XzStream::init(|s| unsafe { l::lzma_alone_decoder(s, memlimit) })
    }

    /// `lzma_raw_decoder`: a headerless decoder with a filter chain.
    pub fn raw_decoder(chain: &[Filter]) -> Result<XzStream, XzError> {
        let chain = Chain::new(chain);
        // SAFETY: as for `stream_encoder`.
        XzStream::init(|s| unsafe { l::lzma_raw_decoder(s, chain.as_ptr()) })
    }

    /// `lzma_code` with `LZMA_RUN` (`finish == false`) or `LZMA_FINISH`.
    pub fn code(&mut self, finish: bool, input: &[u8], output: &mut [u8]) -> XzStep {
        let s = &mut *self.strm;
        s.next_in = input.as_ptr();
        s.avail_in = input.len();
        s.next_out = output.as_mut_ptr();
        s.avail_out = output.len();
        let action = if finish { l::LZMA_FINISH } else { l::LZMA_RUN };
        // SAFETY: the windows point into live slices of the advertised lengths.
        let code = unsafe { l::lzma_code(s, action) };
        let step = XzStep { status: ret(code), consumed: input.len() - s.avail_in, produced: output.len() - s.avail_out };
        s.next_in = ptr::null();
        s.avail_in = 0;
        s.next_out = ptr::null_mut();
        s.avail_out = 0;
        step
    }

    /// `lzma_get_check`: the stream's integrity check id once known.
    pub fn check(&self) -> u32 {
        // SAFETY: the stream is initialised.
        unsafe { l::lzma_get_check(&*self.strm) as u32 }
    }
}

impl Drop for XzStream {
    fn drop(&mut self) {
        // SAFETY: `lzma_end` accepts a stream with or without internal state and frees it once.
        unsafe { l::lzma_end(&mut *self.strm) }
    }
}

/// `lzma_check_is_supported`.
pub fn check_is_supported(check: u32) -> bool {
    // SAFETY: a pure query.
    unsafe { l::lzma_check_is_supported(check as _) != 0 }
}

/// `lzma_properties_size` + `lzma_properties_encode`: the filter's options as the bytes an `.xz`
/// block header stores (without the filter id).
pub fn encode_filter_properties(filter: &Filter) -> Result<Vec<u8>, XzError> {
    let chain = Chain::new(std::slice::from_ref(filter));
    let mut size = 0u32;
    // SAFETY: the chain's first entry is a valid filter.
    ret(unsafe { l::lzma_properties_size(&mut size, chain.as_ptr()) })?;
    let mut props = vec![0u8; size as usize];
    // SAFETY: `props` has the advertised size.
    ret(unsafe { l::lzma_properties_encode(chain.as_ptr(), props.as_mut_ptr()) })?;
    Ok(props)
}

/// `lzma_properties_decode`: the filter with id `id` whose options are encoded in `props`.
pub fn decode_filter_properties(id: u64, props: &[u8]) -> Result<Filter, XzError> {
    let mut filters = [l::lzma_filter { id, options: ptr::null_mut() }, l::lzma_filter { id: VLI_UNKNOWN, options: ptr::null_mut() }];
    // SAFETY: the filter is live; `props` is a live slice.
    ret(unsafe { l::lzma_properties_decode(&mut filters[0], ptr::null(), props.as_ptr(), props.len()) })?;
    let raw = filters[0].options;
    let options = match id {
        FILTER_LZMA1 | FILTER_LZMA2 => {
            // SAFETY: a successful decode of an LZMA filter allocates an `lzma_options_lzma`.
            let o = unsafe { &*(raw as *const l::lzma_options_lzma) };
            FilterOptions::Lzma(LzmaOptions {
                dict_size: o.dict_size,
                lc: o.lc,
                lp: o.lp,
                pb: o.pb,
                mode: o.mode as u32,
                nice_len: o.nice_len,
                mf: o.mf as u32,
                depth: o.depth,
            })
        }
        FILTER_DELTA => {
            // SAFETY: a successful decode of the delta filter allocates an `lzma_options_delta`.
            let o = unsafe { &*(raw as *const l::lzma_options_delta) };
            FilterOptions::Delta { dist: o.dist }
        }
        _ => {
            let start_offset = if raw.is_null() {
                None
            } else {
                // SAFETY: BCJ filters allocate an `lzma_options_bcj` when the properties carry an offset.
                Some(unsafe { (*(raw as *const l::lzma_options_bcj)).start_offset })
            };
            FilterOptions::Bcj { start_offset }
        }
    };
    // SAFETY: frees the options liblzma allocated; the array is terminated.
    unsafe { l::lzma_filters_free(filters.as_mut_ptr(), ptr::null()) };
    Ok(Filter { id, options })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(mut enc: XzStream, mut dec: XzStream, data: &[u8]) {
        let mut out = vec![0u8; data.len() + 4096];
        let step = enc.code(true, data, &mut out);
        assert_eq!(step.status, Ok(XzStatus::StreamEnd));
        out.truncate(step.produced);
        let mut plain = vec![0u8; data.len() + 16];
        let step = dec.code(false, &out, &mut plain);
        assert_eq!(&plain[..step.produced], data);
    }

    #[test]
    fn xz_roundtrip() {
        let data = b"hello hello hello hello lumen".repeat(50);
        roundtrip(XzStream::easy_encoder(6, CHECK_CRC64).unwrap(), XzStream::auto_decoder(u64::MAX, 0).unwrap(), &data);
    }

    #[test]
    fn properties_roundtrip() {
        let o = LzmaOptions::preset(6).unwrap();
        let f = Filter { id: FILTER_LZMA2, options: FilterOptions::Lzma(o) };
        let props = encode_filter_properties(&f).unwrap();
        let back = decode_filter_properties(FILTER_LZMA2, &props).unwrap();
        assert!(matches!(back.options, FilterOptions::Lzma(b) if b.dict_size >= o.dict_size));
    }
}
