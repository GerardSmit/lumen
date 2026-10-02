//! xz / lzma in pure Rust: the `.xz`, legacy `.lzma` ("alone") and raw container formats, LZMA1 /
//! LZMA2 with the delta and BCJ filters, integrity checks, presets and memory limits.
//!
//! The decoder is written here (`lzma.rs`, `filter.rs`): Python's `_lzma` feeds it arbitrary
//! pieces of input and needs to know exactly how much of each piece belongs to the stream, which
//! the pull-style decoders of the available crates cannot do. The LZMA / LZMA2 *encoders* come
//! from the `lzma-rust2` crate; the containers, filter chains and checks around them are here.
//!
//! The API keeps the streaming shape of liblzma (explicit input / output windows, the library's
//! status codes), which is what Python's `_lzma` is specified against.

use std::io::Write;

use lzma_rust2 as lr;
use sha2::{Digest, Sha256};

mod filter;
mod lzma;

use filter::Filter as FilterStage;

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

pub const TELL_NO_CHECK: u32 = 0x01;
pub const TELL_UNSUPPORTED_CHECK: u32 = 0x02;
pub const TELL_ANY_CHECK: u32 = 0x04;
/// Decoder flags Python's decompressor uses.
pub const TELL_CHECKS: u32 = TELL_ANY_CHECK | TELL_NO_CHECK;

const DICT_MIN: u32 = 4096;
const DICT_MAX: u32 = (1 << 30) + (1 << 29);

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

/// The LZMA parameters of a filter (`lzma_options_lzma` without the preset dictionary).
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
        let level = (preset & 0x1F) as usize;
        let flags = preset & !0x1F;
        if level > 9 || flags & !PRESET_EXTREME != 0 {
            return None;
        }
        const DICT_POW2: [u32; 10] = [18, 20, 21, 22, 22, 23, 23, 24, 25, 26];
        const DEPTHS: [u32; 4] = [4, 8, 24, 48];
        let mut o = LzmaOptions { dict_size: 1 << DICT_POW2[level], lc: 3, lp: 0, pb: 2, mode: MODE_FAST, nice_len: 0, mf: 0, depth: 0 };
        if level <= 3 {
            o.mode = MODE_FAST;
            o.mf = if level == 0 { MF_HC3 } else { MF_HC4 };
            o.nice_len = if level <= 1 { 128 } else { 273 };
            o.depth = DEPTHS[level];
        } else {
            o.mode = MODE_NORMAL;
            o.mf = MF_BT4;
            o.nice_len = match level {
                4 => 16,
                5 => 32,
                _ => 64,
            };
            o.depth = 0;
        }
        if flags & PRESET_EXTREME != 0 {
            o.mode = MODE_NORMAL;
            o.mf = MF_BT4;
            if level == 3 || level == 5 {
                o.nice_len = 192;
                o.depth = 0;
            } else {
                o.nice_len = 273;
                o.depth = 512;
            }
        }
        Some(o)
    }

    fn validate(&self) -> Result<(), XzError> {
        let ok = (DICT_MIN..=DICT_MAX).contains(&self.dict_size)
            && self.lc <= 4
            && self.lp <= 4
            && self.lc + self.lp <= 4
            && self.pb <= 4
            && matches!(self.mode, MODE_FAST | MODE_NORMAL)
            && matches!(self.mf, MF_HC3 | MF_HC4 | MF_BT2 | MF_BT3 | MF_BT4)
            && (2..=273).contains(&self.nice_len);
        if ok {
            Ok(())
        } else {
            Err(XzError::Options)
        }
    }

    fn to_encoder(self) -> lr::LzmaOptions {
        let mode = if self.mode == MODE_FAST { lr::EncodeMode::Fast } else { lr::EncodeMode::Normal };
        let mf = if matches!(self.mf, MF_HC3 | MF_HC4) { lr::MfType::Hc4 } else { lr::MfType::Bt4 };
        lr::LzmaOptions::new(self.dict_size, self.lc, self.lp, self.pb, mode, self.nice_len.clamp(8, 273), mf, self.depth as i32)
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

fn is_lzma(id: u64) -> bool {
    id == FILTER_LZMA1 || id == FILTER_LZMA2
}

/// A chain is valid when it has at most [`FILTERS_MAX`] entries, ends in an LZMA filter and has
/// only delta / BCJ filters before it. `xz` additionally needs the last one to be LZMA2.
fn validate_chain(chain: &[Filter], xz: bool) -> Result<(), XzError> {
    let Some((last, init)) = chain.split_last() else { return Err(XzError::Options) };
    if chain.len() > FILTERS_MAX || !is_lzma(last.id) || (xz && last.id != FILTER_LZMA2) {
        return Err(XzError::Options);
    }
    for f in init {
        if !FilterStage::is_filter_id(f.id) {
            return Err(XzError::Options);
        }
    }
    for f in chain {
        match (f.id, f.options) {
            (FILTER_LZMA1 | FILTER_LZMA2, FilterOptions::Lzma(o)) => o.validate()?,
            (FILTER_DELTA, FilterOptions::Delta { dist }) if (1..=256).contains(&dist) => {}
            (id, FilterOptions::Bcj { .. }) if id != FILTER_DELTA && FilterStage::is_filter_id(id) => {}
            _ => return Err(XzError::Options),
        }
    }
    Ok(())
}

// ---- integrity checks ---------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

const CRC64_TABLE: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u64;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xC96C_5795_D787_0F42 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC32_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

enum Checker {
    None,
    Crc32(u32),
    Crc64(u64),
    Sha256(Box<Sha256>),
}

impl Checker {
    /// The checker for check id `id`; ids this build cannot compute are not verified.
    fn new(id: u32) -> Checker {
        match id {
            CHECK_CRC32 => Checker::Crc32(!0),
            CHECK_CRC64 => Checker::Crc64(!0),
            CHECK_SHA256 => Checker::Sha256(Box::new(Sha256::new())),
            _ => Checker::None,
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Checker::None => {}
            Checker::Crc32(c) => {
                for &b in data {
                    *c = CRC32_TABLE[((*c ^ u32::from(b)) & 0xFF) as usize] ^ (*c >> 8);
                }
            }
            Checker::Crc64(c) => {
                for &b in data {
                    *c = CRC64_TABLE[((*c ^ u64::from(b)) & 0xFF) as usize] ^ (*c >> 8);
                }
            }
            Checker::Sha256(h) => h.update(data),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Checker::None => Vec::new(),
            Checker::Crc32(c) => (!c).to_le_bytes().to_vec(),
            Checker::Crc64(c) => (!c).to_le_bytes().to_vec(),
            Checker::Sha256(h) => h.finalize().to_vec(),
        }
    }
}

const CHECK_SIZES: [usize; 16] = [0, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64];

/// `lzma_check_is_supported`.
pub fn check_is_supported(check: u32) -> bool {
    matches!(check, CHECK_NONE | CHECK_CRC32 | CHECK_CRC64 | CHECK_SHA256)
}

// ---- variable-length integers --------------------------------------------------------------

fn put_vli(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_vli(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return if i > 0 && b == 0 { None } else { Some(v) };
        }
    }
    None
}

// ---- filter properties ---------------------------------------------------------------------

fn lzma2_dict_byte(dict_size: u32) -> u8 {
    for b in 0..40u32 {
        if (u64::from(2 | (b & 1)) << (b / 2 + 11)) >= u64::from(dict_size) {
            return b as u8;
        }
    }
    40
}

fn lzma2_dict_size(byte: u8) -> Option<u32> {
    match byte {
        0..=39 => Some((2 | (u32::from(byte) & 1)) << (byte / 2 + 11)),
        40 => Some(u32::MAX),
        _ => None,
    }
}

/// The filter's options as the bytes an `.xz` block header stores (without the filter id).
pub fn encode_filter_properties(filter: &Filter) -> Result<Vec<u8>, XzError> {
    match (filter.id, filter.options) {
        (FILTER_LZMA2, FilterOptions::Lzma(o)) => {
            if o.dict_size < DICT_MIN {
                return Err(XzError::Options);
            }
            Ok(vec![lzma2_dict_byte(o.dict_size)])
        }
        (FILTER_LZMA1, FilterOptions::Lzma(o)) => {
            if o.lc > 8 || o.lp > 4 || o.pb > 4 {
                return Err(XzError::Options);
            }
            let mut v = vec![((o.pb * 5 + o.lp) * 9 + o.lc) as u8];
            v.extend_from_slice(&o.dict_size.to_le_bytes());
            Ok(v)
        }
        (FILTER_DELTA, FilterOptions::Delta { dist }) if (1..=256).contains(&dist) => Ok(vec![(dist - 1) as u8]),
        (id, FilterOptions::Bcj { start_offset }) if id != FILTER_DELTA && FilterStage::is_filter_id(id) => match start_offset {
            None | Some(0) => Ok(Vec::new()),
            Some(n) => Ok(n.to_le_bytes().to_vec()),
        },
        _ => Err(XzError::Options),
    }
}

/// The filter with id `id` whose options are encoded in `props`.
pub fn decode_filter_properties(id: u64, props: &[u8]) -> Result<Filter, XzError> {
    let base = LzmaOptions::preset(PRESET_DEFAULT).ok_or(XzError::Prog)?;
    let options = match id {
        FILTER_LZMA2 => {
            let [b] = props else { return Err(XzError::Options) };
            let dict_size = lzma2_dict_size(*b).ok_or(XzError::Options)?;
            FilterOptions::Lzma(LzmaOptions { dict_size, ..base })
        }
        FILTER_LZMA1 => {
            let [p, d0, d1, d2, d3] = props else { return Err(XzError::Options) };
            let p = u32::from(*p);
            if p >= 9 * 5 * 5 {
                return Err(XzError::Options);
            }
            let (lc, rest) = (p % 9, p / 9);
            FilterOptions::Lzma(LzmaOptions { dict_size: u32::from_le_bytes([*d0, *d1, *d2, *d3]), lc, lp: rest % 5, pb: rest / 5, ..base })
        }
        FILTER_DELTA => {
            let [d] = props else { return Err(XzError::Options) };
            FilterOptions::Delta { dist: u32::from(*d) + 1 }
        }
        id if FilterStage::is_filter_id(id) => match props {
            [] => FilterOptions::Bcj { start_offset: None },
            [a, b, c, d] => FilterOptions::Bcj { start_offset: Some(u32::from_le_bytes([*a, *b, *c, *d])) },
            _ => return Err(XzError::Options),
        },
        _ => return Err(XzError::Options),
    };
    Ok(Filter { id, options })
}

// ---- the stream ------------------------------------------------------------------------------

/// What one call to [`XzStream::code`] did.
#[derive(Clone, Copy, Debug)]
pub struct XzStep {
    pub status: Result<XzStatus, XzError>,
    pub consumed: usize,
    pub produced: usize,
}

/// An encoder or a decoder depending on the constructor, driven like an `lzma_stream`.
pub struct XzStream {
    imp: Imp,
}

enum Imp {
    Enc(Box<Encoder>),
    Dec(Box<Decoder>),
}

impl XzStream {
    /// `lzma_easy_encoder`: an `.xz` encoder at `preset` with `check`.
    pub fn easy_encoder(preset: u32, check: u32) -> Result<XzStream, XzError> {
        let options = LzmaOptions::preset(preset).ok_or(XzError::Options)?;
        XzStream::stream_encoder(&[Filter { id: FILTER_LZMA2, options: FilterOptions::Lzma(options) }], check)
    }

    /// `lzma_stream_encoder`: an `.xz` encoder with a custom filter chain.
    pub fn stream_encoder(chain: &[Filter], check: u32) -> Result<XzStream, XzError> {
        if check > CHECK_ID_MAX {
            return Err(XzError::Options);
        }
        if !check_is_supported(check) {
            return Err(XzError::UnsupportedCheck);
        }
        validate_chain(chain, true)?;
        let mut enc = Encoder::new(Wrap::Xz, chain, check);
        enc.pending.extend_from_slice(&[0xFD, b'7', b'z', b'X', b'Z', 0]);
        let flags = [0u8, check as u8];
        enc.pending.extend_from_slice(&flags);
        enc.pending.extend_from_slice(&crc32(&flags).to_le_bytes());
        enc.hdr_flags = flags;
        Ok(XzStream { imp: Imp::Enc(Box::new(enc)) })
    }

    /// `lzma_alone_encoder`: a legacy `.lzma` encoder.
    pub fn alone_encoder(options: &LzmaOptions) -> Result<XzStream, XzError> {
        options.validate()?;
        let chain = [Filter { id: FILTER_LZMA1, options: FilterOptions::Lzma(*options) }];
        let mut enc = Encoder::new(Wrap::Alone, &chain, CHECK_NONE);
        enc.start_core()?;
        Ok(XzStream { imp: Imp::Enc(Box::new(enc)) })
    }

    /// `lzma_raw_encoder`: a headerless encoder with a filter chain.
    pub fn raw_encoder(chain: &[Filter]) -> Result<XzStream, XzError> {
        validate_chain(chain, false)?;
        let mut enc = Encoder::new(Wrap::Raw, chain, CHECK_NONE);
        enc.start_core()?;
        Ok(XzStream { imp: Imp::Enc(Box::new(enc)) })
    }

    /// `lzma_auto_decoder`: `.xz` or `.lzma`, detected from the header.
    pub fn auto_decoder(memlimit: u64, flags: u32) -> Result<XzStream, XzError> {
        Ok(XzStream { imp: Imp::Dec(Box::new(Decoder::new(Format::Auto, memlimit, flags, Stage::Auto))) })
    }

    /// `lzma_stream_decoder`: `.xz` only.
    pub fn stream_decoder(memlimit: u64, flags: u32) -> Result<XzStream, XzError> {
        Ok(XzStream { imp: Imp::Dec(Box::new(Decoder::new(Format::Xz, memlimit, flags, Stage::StreamHeader))) })
    }

    /// `lzma_alone_decoder`: `.lzma` only.
    pub fn alone_decoder(memlimit: u64) -> Result<XzStream, XzError> {
        Ok(XzStream { imp: Imp::Dec(Box::new(Decoder::new(Format::Alone, memlimit, 0, Stage::AloneHeader))) })
    }

    /// `lzma_raw_decoder`: a headerless decoder with a filter chain.
    pub fn raw_decoder(chain: &[Filter]) -> Result<XzStream, XzError> {
        validate_chain(chain, false)?;
        let mut dec = Decoder::new(Format::Raw, u64::MAX, 0, Stage::Pipe);
        dec.pipe = Some(Pipe::new(chain, None)?);
        Ok(XzStream { imp: Imp::Dec(Box::new(dec)) })
    }

    /// `lzma_code` with `LZMA_RUN` (`finish == false`) or `LZMA_FINISH`.
    pub fn code(&mut self, finish: bool, input: &[u8], output: &mut [u8]) -> XzStep {
        match &mut self.imp {
            Imp::Enc(e) => e.code(finish, input, output),
            Imp::Dec(d) => d.code(finish, input, output),
        }
    }

    /// `lzma_get_check`: the stream's integrity check id once known.
    pub fn check(&self) -> u32 {
        match &self.imp {
            Imp::Enc(e) => e.check,
            Imp::Dec(d) => d.check_id,
        }
    }
}

// ---- encoder ----------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrap {
    Xz,
    Alone,
    Raw,
}

enum Core {
    L2(lr::Lzma2Writer<Vec<u8>>),
    L1(lr::LzmaWriter<Vec<u8>>),
}

impl Core {
    fn write(&mut self, data: &[u8]) -> Result<(), XzError> {
        let r = match self {
            Core::L2(w) => w.write_all(data),
            Core::L1(w) => w.write_all(data),
        };
        r.map_err(|_| XzError::Prog)
    }

    fn sink(&mut self) -> &mut Vec<u8> {
        match self {
            Core::L2(w) => w.inner_mut(),
            Core::L1(w) => w.inner_mut(),
        }
    }

    fn finish(self) -> Result<Vec<u8>, XzError> {
        match self {
            Core::L2(w) => w.finish(),
            Core::L1(w) => w.finish(),
        }
        .map_err(|_| XzError::Prog)
    }
}

struct Encoder {
    wrap: Wrap,
    chain: Vec<Filter>,
    check: u32,
    hdr_flags: [u8; 2],
    stages: Vec<FilterStage>,
    core: Option<Core>,
    checker: Checker,
    pending: Vec<u8>,
    ppos: usize,
    block_started: bool,
    finished: bool,
    header_size: u64,
    compressed: u64,
    uncompressed: u64,
}

impl Encoder {
    fn new(wrap: Wrap, chain: &[Filter], check: u32) -> Encoder {
        Encoder {
            wrap,
            chain: chain.to_vec(),
            check,
            hdr_flags: [0; 2],
            stages: Vec::new(),
            core: None,
            checker: Checker::new(check),
            pending: Vec::new(),
            ppos: 0,
            block_started: false,
            finished: false,
            header_size: 0,
            compressed: 0,
            uncompressed: 0,
        }
    }

    fn lzma_options(&self) -> LzmaOptions {
        match self.chain.last().map(|f| f.options) {
            Some(FilterOptions::Lzma(o)) => o,
            _ => unreachable!("validated chains end in an LZMA filter"),
        }
    }

    fn start_core(&mut self) -> Result<(), XzError> {
        let opts = self.lzma_options().to_encoder();
        let core = match (self.wrap, self.chain.last().map(|f| f.id)) {
            (Wrap::Alone, _) => Core::L1(lr::LzmaWriter::new_use_header(Vec::new(), &opts, None).map_err(|_| XzError::Options)?),
            (_, Some(FILTER_LZMA1)) => Core::L1(lr::LzmaWriter::new_no_header(Vec::new(), &opts, true).map_err(|_| XzError::Options)?),
            _ => Core::L2(lr::Lzma2Writer::new(Vec::new(), lr::Lzma2Options { lzma_options: opts, chunk_size: None })),
        };
        self.core = Some(core);
        self.stages = self
            .chain
            .iter()
            .filter(|f| !is_lzma(f.id))
            .map(|f| match f.options {
                FilterOptions::Delta { dist } => Ok(FilterStage::delta(dist, true)),
                FilterOptions::Bcj { start_offset } => FilterStage::bcj(f.id, start_offset.unwrap_or(0), true).ok_or(XzError::Options),
                FilterOptions::Lzma(_) => Err(XzError::Options),
            })
            .collect::<Result<_, _>>()?;
        self.drain_core();
        Ok(())
    }

    fn start_block(&mut self) -> Result<(), XzError> {
        let mut h = vec![0u8, (self.chain.len() - 1) as u8];
        for f in &self.chain {
            put_vli(&mut h, f.id);
            let props = encode_filter_properties(f)?;
            put_vli(&mut h, props.len() as u64);
            h.extend_from_slice(&props);
        }
        while (h.len() + 4) % 4 != 0 {
            h.push(0);
        }
        h[0] = ((h.len() + 4) / 4 - 1) as u8;
        let crc = crc32(&h);
        h.extend_from_slice(&crc.to_le_bytes());
        self.header_size = h.len() as u64;
        self.pending.extend_from_slice(&h);
        self.block_started = true;
        self.start_core()
    }

    fn drain_core(&mut self) {
        if let Some(core) = &mut self.core {
            let out = std::mem::take(core.sink());
            self.compressed += out.len() as u64;
            self.pending.extend_from_slice(&out);
        }
    }

    fn push(&mut self, input: &[u8]) -> Result<(), XzError> {
        if self.wrap == Wrap::Xz && !self.block_started {
            self.start_block()?;
        }
        self.checker.update(input);
        self.uncompressed += input.len() as u64;
        let mut data = input.to_vec();
        for s in &mut self.stages {
            data = s.run(data, false);
        }
        self.core.as_mut().ok_or(XzError::Prog)?.write(&data)?;
        self.drain_core();
        Ok(())
    }

    fn finish_all(&mut self) -> Result<(), XzError> {
        if let Some(mut core) = self.core.take() {
            let mut data = Vec::new();
            for s in &mut self.stages {
                data = s.run(data, true);
            }
            core.write(&data)?;
            let rest = core.finish()?;
            self.compressed += rest.len() as u64;
            self.pending.extend_from_slice(&rest);
        }
        if self.wrap != Wrap::Xz {
            return Ok(());
        }
        let mut records = Vec::new();
        if self.block_started {
            let pad = (4 - self.compressed % 4) % 4;
            self.pending.extend(std::iter::repeat(0).take(pad as usize));
            let checker = std::mem::replace(&mut self.checker, Checker::None);
            self.pending.extend_from_slice(&checker.finish());
            let unpadded = self.header_size + self.compressed + CHECK_SIZES[self.check as usize] as u64;
            records.push((unpadded, self.uncompressed));
        }
        let mut index = vec![0u8];
        put_vli(&mut index, records.len() as u64);
        for (unpadded, uncompressed) in &records {
            put_vli(&mut index, *unpadded);
            put_vli(&mut index, *uncompressed);
        }
        while (index.len() + 4) % 4 != 0 {
            index.push(0);
        }
        let crc = crc32(&index);
        index.extend_from_slice(&crc.to_le_bytes());
        self.pending.extend_from_slice(&index);
        let mut footer = Vec::new();
        footer.extend_from_slice(&((index.len() / 4 - 1) as u32).to_le_bytes());
        footer.extend_from_slice(&self.hdr_flags);
        self.pending.extend_from_slice(&crc32(&footer).to_le_bytes());
        self.pending.extend_from_slice(&footer);
        self.pending.extend_from_slice(b"YZ");
        Ok(())
    }

    fn code(&mut self, finish: bool, input: &[u8], output: &mut [u8]) -> XzStep {
        let fail = |e: XzError| XzStep { status: Err(e), consumed: 0, produced: 0 };
        if !input.is_empty() {
            if self.finished {
                return fail(XzError::Prog);
            }
            if let Err(e) = self.push(input) {
                return fail(e);
            }
        }
        if finish && !self.finished {
            if let Err(e) = self.finish_all() {
                return fail(e);
            }
            self.finished = true;
        }
        let n = (self.pending.len() - self.ppos).min(output.len());
        output[..n].copy_from_slice(&self.pending[self.ppos..self.ppos + n]);
        self.ppos += n;
        if self.ppos == self.pending.len() {
            self.pending.clear();
            self.ppos = 0;
        }
        let done = self.finished && self.pending.is_empty();
        XzStep { status: Ok(if done { XzStatus::StreamEnd } else { XzStatus::Ok }), consumed: input.len(), produced: n }
    }
}

// ---- decoder ----------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Auto,
    Xz,
    Alone,
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Auto,
    StreamHeader,
    BlockHeaderSize,
    BlockHeader,
    BlockData,
    BlockPad,
    BlockCheck,
    Index,
    Footer,
    AloneHeader,
    Pipe,
    Done,
}

enum CoreDec {
    L1(lzma::Lzma1),
    L2(lzma::Lzma2),
}

/// The LZMA decoder and the filters that undo the rest of a chain.
struct Pipe {
    core: CoreDec,
    stages: Vec<FilterStage>,
}

impl Pipe {
    fn new(chain: &[Filter], alone_size: Option<u64>) -> Result<Pipe, XzError> {
        let mut stages = Vec::new();
        let mut core = None;
        for f in chain {
            match (f.id, f.options) {
                (FILTER_LZMA2, FilterOptions::Lzma(o)) => core = Some(CoreDec::L2(lzma::Lzma2::new(o.dict_size))),
                (FILTER_LZMA1, FilterOptions::Lzma(o)) => core = Some(CoreDec::L1(lzma::Lzma1::new(o.dict_size, o.lc, o.lp, o.pb, alone_size))),
                (FILTER_DELTA, FilterOptions::Delta { dist }) => stages.push(FilterStage::delta(dist, false)),
                (id, FilterOptions::Bcj { start_offset }) => stages.push(FilterStage::bcj(id, start_offset.unwrap_or(0), false).ok_or(XzError::Options)?),
                _ => return Err(XzError::Options),
            }
        }
        Ok(Pipe { core: core.ok_or(XzError::Options)?, stages })
    }

    /// Decodes from `data`, appending final bytes to `out`; returns the input used and whether
    /// the LZMA stream ended.
    fn run(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(usize, bool), XzError> {
        let mut raw = Vec::new();
        let (used, ended) = match &mut self.core {
            CoreDec::L1(d) => d.feed(data, &mut raw)?,
            CoreDec::L2(d) => d.feed(data, &mut raw)?,
        };
        for s in self.stages.iter_mut().rev() {
            raw = s.run(raw, ended);
        }
        out.extend_from_slice(&raw);
        Ok((used, ended))
    }
}

struct Block {
    pipe: Pipe,
    header_size: u64,
    compressed: u64,
    uncompressed: u64,
    declared_compressed: Option<u64>,
    declared_uncompressed: Option<u64>,
    checker: Checker,
}

#[derive(Default)]
struct IndexParse {
    raw: Vec<u8>,
    value: u64,
    shift: u32,
    count: Option<u64>,
    numbers: Vec<u64>,
}

struct Decoder {
    format: Format,
    memlimit: u64,
    flags: u32,
    stage: Stage,
    buf: Vec<u8>,
    need: usize,
    ready: Vec<u8>,
    rpos: usize,
    check_id: u32,
    hdr_flags: [u8; 2],
    block: Option<Block>,
    pipe: Option<Pipe>,
    records: Vec<(u64, u64)>,
    index: IndexParse,
    pad_left: u64,
    failed: Option<XzError>,
}

enum Step {
    Progress,
    Need,
    Report(XzStatus),
}

impl Decoder {
    fn new(format: Format, memlimit: u64, flags: u32, stage: Stage) -> Decoder {
        Decoder {
            format,
            memlimit: memlimit.max(1),
            flags,
            stage,
            buf: Vec::new(),
            need: 0,
            ready: Vec::new(),
            rpos: 0,
            check_id: 0,
            hdr_flags: [0; 2],
            block: None,
            pipe: None,
            records: Vec::new(),
            index: IndexParse::default(),
            pad_left: 0,
            failed: None,
        }
    }

    fn code(&mut self, finish: bool, input: &[u8], output: &mut [u8]) -> XzStep {
        if let Some(e) = self.failed {
            return XzStep { status: Err(e), consumed: 0, produced: 0 };
        }
        let (mut pos, mut produced) = (0usize, 0usize);
        let status = loop {
            let n = (self.ready.len() - self.rpos).min(output.len() - produced);
            output[produced..produced + n].copy_from_slice(&self.ready[self.rpos..self.rpos + n]);
            self.rpos += n;
            produced += n;
            if self.rpos == self.ready.len() {
                self.ready.clear();
                self.rpos = 0;
            }
            if !self.ready.is_empty() {
                break Ok(XzStatus::Ok);
            }
            if self.stage == Stage::Done {
                break Ok(XzStatus::StreamEnd);
            }
            if produced == output.len() && !output.is_empty() {
                break Ok(XzStatus::Ok);
            }
            match self.step(input, &mut pos) {
                Ok(Step::Progress) => {}
                Ok(Step::Need) => {
                    break if finish && pos == input.len() && produced == 0 { Err(XzError::Buf) } else { Ok(XzStatus::Ok) };
                }
                Ok(Step::Report(s)) => break Ok(s),
                Err(e) => {
                    self.failed = Some(e);
                    break Err(e);
                }
            }
        };
        XzStep { status, consumed: pos, produced }
    }

    /// Collects `want` bytes into `buf`; true once they are all there.
    fn fill(&mut self, input: &[u8], pos: &mut usize, want: usize) -> bool {
        let take = (want - self.buf.len()).min(input.len() - *pos);
        self.buf.extend_from_slice(&input[*pos..*pos + take]);
        *pos += take;
        self.buf.len() == want
    }

    fn step(&mut self, input: &[u8], pos: &mut usize) -> Result<Step, XzError> {
        match self.stage {
            Stage::Auto => {
                let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
                if b == 0xFD {
                    self.stage = Stage::StreamHeader;
                    return Ok(Step::Progress);
                }
                self.stage = Stage::AloneHeader;
                if self.flags & TELL_NO_CHECK != 0 {
                    return Ok(Step::Report(XzStatus::NoCheck));
                }
                Ok(Step::Progress)
            }
            Stage::StreamHeader => {
                if !self.fill(input, pos, 12) {
                    return Ok(Step::Need);
                }
                let h = std::mem::take(&mut self.buf);
                if h[..6] != [0xFD, b'7', b'z', b'X', b'Z', 0] {
                    return Err(XzError::Format);
                }
                if crc32(&h[6..8]) != u32::from_le_bytes([h[8], h[9], h[10], h[11]]) {
                    return Err(XzError::Data);
                }
                if h[6] != 0 || h[7] & 0xF0 != 0 {
                    return Err(XzError::Options);
                }
                self.hdr_flags = [h[6], h[7]];
                self.check_id = u32::from(h[7] & 0x0F);
                self.stage = Stage::BlockHeaderSize;
                if self.check_id == CHECK_NONE && self.flags & TELL_NO_CHECK != 0 {
                    return Ok(Step::Report(XzStatus::NoCheck));
                }
                if !check_is_supported(self.check_id) && self.flags & TELL_UNSUPPORTED_CHECK != 0 {
                    return Err(XzError::UnsupportedCheck);
                }
                if self.flags & TELL_ANY_CHECK != 0 {
                    return Ok(Step::Report(XzStatus::GetCheck));
                }
                Ok(Step::Progress)
            }
            Stage::BlockHeaderSize => {
                let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
                *pos += 1;
                if b == 0 {
                    self.index = IndexParse { raw: vec![0], ..IndexParse::default() };
                    self.stage = Stage::Index;
                } else {
                    self.need = (usize::from(b) + 1) * 4;
                    self.buf = vec![b];
                    self.stage = Stage::BlockHeader;
                }
                Ok(Step::Progress)
            }
            Stage::BlockHeader => {
                if !self.fill(input, pos, self.need) {
                    return Ok(Step::Need);
                }
                let h = std::mem::take(&mut self.buf);
                self.block = Some(self.parse_block_header(&h)?);
                self.stage = Stage::BlockData;
                Ok(Step::Progress)
            }
            Stage::BlockData => {
                let Some(block) = self.block.as_mut() else { return Err(XzError::Prog) };
                let before = self.ready.len();
                let (used, ended) = block.pipe.run(&input[*pos..], &mut self.ready)?;
                *pos += used;
                block.compressed += used as u64;
                let made = &self.ready[before..];
                block.checker.update(made);
                block.uncompressed += made.len() as u64;
                if block.declared_compressed.is_some_and(|n| block.compressed > n) || block.declared_uncompressed.is_some_and(|n| block.uncompressed > n) {
                    return Err(XzError::Data);
                }
                if ended {
                    if block.declared_compressed.is_some_and(|n| n != block.compressed) || block.declared_uncompressed.is_some_and(|n| n != block.uncompressed) {
                        return Err(XzError::Data);
                    }
                    self.pad_left = (4 - block.compressed % 4) % 4;
                    self.stage = Stage::BlockPad;
                    return Ok(Step::Progress);
                }
                if used == 0 && made.is_empty() {
                    return Ok(Step::Need);
                }
                Ok(Step::Progress)
            }
            Stage::BlockPad => {
                while self.pad_left > 0 {
                    let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
                    *pos += 1;
                    if b != 0 {
                        return Err(XzError::Data);
                    }
                    self.pad_left -= 1;
                }
                self.need = CHECK_SIZES[self.check_id as usize];
                self.buf.clear();
                self.stage = Stage::BlockCheck;
                Ok(Step::Progress)
            }
            Stage::BlockCheck => {
                if !self.fill(input, pos, self.need) {
                    return Ok(Step::Need);
                }
                let got = std::mem::take(&mut self.buf);
                let Some(block) = self.block.take() else { return Err(XzError::Prog) };
                if check_is_supported(self.check_id) && block.checker.finish() != got {
                    return Err(XzError::Data);
                }
                let unpadded = block.header_size + block.compressed + got.len() as u64;
                self.records.push((unpadded, block.uncompressed));
                self.stage = Stage::BlockHeaderSize;
                Ok(Step::Progress)
            }
            Stage::Index => self.index_step(input, pos),
            Stage::Footer => {
                if !self.fill(input, pos, 12) {
                    return Ok(Step::Need);
                }
                let f = std::mem::take(&mut self.buf);
                let backward = u64::from(u32::from_le_bytes([f[4], f[5], f[6], f[7]]));
                if crc32(&f[4..10]) != u32::from_le_bytes([f[0], f[1], f[2], f[3]]) || f[10..12] != *b"YZ" {
                    return Err(XzError::Data);
                }
                if (backward + 1) * 4 != self.index.raw.len() as u64 || f[8..10] != self.hdr_flags {
                    return Err(XzError::Data);
                }
                self.stage = Stage::Done;
                Ok(Step::Progress)
            }
            Stage::AloneHeader => {
                if !self.fill(input, pos, 13) {
                    return Ok(Step::Need);
                }
                let h = std::mem::take(&mut self.buf);
                self.pipe = Some(self.parse_alone_header(&h)?);
                self.stage = Stage::Pipe;
                Ok(Step::Progress)
            }
            Stage::Pipe => {
                let Some(pipe) = self.pipe.as_mut() else { return Err(XzError::Prog) };
                let before = self.ready.len();
                let (used, ended) = pipe.run(&input[*pos..], &mut self.ready)?;
                *pos += used;
                if ended {
                    self.stage = Stage::Done;
                    return Ok(Step::Progress);
                }
                if used == 0 && self.ready.len() == before {
                    return Ok(Step::Need);
                }
                Ok(Step::Progress)
            }
            Stage::Done => Ok(Step::Need),
        }
    }

    fn index_step(&mut self, input: &[u8], pos: &mut usize) -> Result<Step, XzError> {
        loop {
            let ix = &mut self.index;
            let count_known = ix.count.is_some();
            let wanted = ix.count.map_or(0, |c| c * 2);
            if count_known && ix.numbers.len() as u64 == wanted {
                if ix.raw.len() % 4 != 0 {
                    let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
                    *pos += 1;
                    if b != 0 {
                        return Err(XzError::Data);
                    }
                    ix.raw.push(0);
                    continue;
                }
                while self.buf.len() < 4 {
                    let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
                    *pos += 1;
                    self.buf.push(b);
                }
                let crc = std::mem::take(&mut self.buf);
                if crc32(&self.index.raw) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
                    return Err(XzError::Data);
                }
                let nums = &self.index.numbers;
                if nums.len() != self.records.len() * 2 || nums.chunks(2).zip(&self.records).any(|(n, r)| n[0] != r.0 || n[1] != r.1) {
                    return Err(XzError::Data);
                }
                self.index.raw.extend_from_slice(&crc);
                self.stage = Stage::Footer;
                return Ok(Step::Progress);
            }
            let Some(&b) = input.get(*pos) else { return Ok(Step::Need) };
            *pos += 1;
            ix.raw.push(b);
            ix.value |= u64::from(b & 0x7F) << ix.shift;
            if b & 0x80 != 0 {
                ix.shift += 7;
                if ix.shift >= 63 {
                    return Err(XzError::Data);
                }
                continue;
            }
            if ix.shift > 0 && b == 0 {
                return Err(XzError::Data);
            }
            let v = std::mem::take(&mut ix.value);
            ix.shift = 0;
            if ix.count.is_none() {
                if v > u64::from(u32::MAX) {
                    return Err(XzError::Data);
                }
                ix.count = Some(v);
            } else {
                ix.numbers.push(v);
            }
        }
    }

    fn memory_needed(chain: &[Filter]) -> u64 {
        chain
            .iter()
            .map(|f| match f.options {
                FilterOptions::Lzma(o) => u64::from(o.dict_size) + (32 << 10),
                _ => 1 << 10,
            })
            .sum()
    }

    fn parse_block_header(&self, h: &[u8]) -> Result<Block, XzError> {
        let size = h.len();
        if crc32(&h[..size - 4]) != u32::from_le_bytes([h[size - 4], h[size - 3], h[size - 2], h[size - 1]]) {
            return Err(XzError::Data);
        }
        let flags = h[1];
        if flags & 0x3C != 0 {
            return Err(XzError::Options);
        }
        let body = &h[..size - 4];
        let mut at = 2usize;
        let mut sized = |present: bool| -> Result<Option<u64>, XzError> {
            if !present {
                return Ok(None);
            }
            match get_vli(body, &mut at) {
                Some(v) if (1..1 << 63).contains(&v) => Ok(Some(v)),
                _ => Err(XzError::Data),
            }
        };
        let declared_compressed = sized(flags & 0x40 != 0)?;
        let declared_uncompressed = sized(flags & 0x80 != 0)?;
        let mut chain = Vec::new();
        for _ in 0..=(flags & 3) {
            let id = get_vli(body, &mut at).ok_or(XzError::Data)?;
            let n = get_vli(body, &mut at).ok_or(XzError::Data)? as usize;
            if n > body.len() - at {
                return Err(XzError::Data);
            }
            chain.push(decode_filter_properties(id, &body[at..at + n])?);
            at += n;
        }
        if body[at..].iter().any(|&b| b != 0) {
            return Err(XzError::Options);
        }
        validate_chain_for_decode(&chain)?;
        if Decoder::memory_needed(&chain) > self.memlimit {
            return Err(XzError::MemLimit);
        }
        Ok(Block {
            pipe: Pipe::new(&chain, None)?,
            header_size: size as u64,
            compressed: 0,
            uncompressed: 0,
            declared_compressed,
            declared_uncompressed,
            checker: Checker::new(self.check_id),
        })
    }

    fn parse_alone_header(&self, h: &[u8]) -> Result<Pipe, XzError> {
        let picky = self.format == Format::Auto;
        let props = u32::from(h[0]);
        if props >= 9 * 5 * 5 {
            return Err(XzError::Format);
        }
        let (lc, rest) = (props % 9, props / 9);
        let (lp, pb) = (rest % 5, rest / 5);
        let dict_size = u32::from_le_bytes([h[1], h[2], h[3], h[4]]);
        let size = u64::from_le_bytes([h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12]]);
        if picky {
            let d = dict_size.wrapping_sub(1);
            let rounded = (d | (d >> 2) | (d >> 3) | (d >> 4) | (d >> 8) | (d >> 16)).wrapping_add(1);
            if dict_size != u32::MAX && rounded != dict_size {
                return Err(XzError::Format);
            }
            if size != u64::MAX && size >= 1 << 38 {
                return Err(XzError::Format);
            }
        }
        if lc + lp > 4 {
            return Err(XzError::Format);
        }
        if u64::from(dict_size) + (32 << 10) > self.memlimit {
            return Err(XzError::MemLimit);
        }
        let options = LzmaOptions { dict_size, lc, lp, pb, ..LzmaOptions::preset(PRESET_DEFAULT).ok_or(XzError::Prog)? };
        let size = if size == u64::MAX { None } else { Some(size) };
        Pipe::new(&[Filter { id: FILTER_LZMA1, options: FilterOptions::Lzma(options) }], size)
    }
}

/// A block's chain: LZMA2 last, delta / BCJ before it.
fn validate_chain_for_decode(chain: &[Filter]) -> Result<(), XzError> {
    let Some((last, init)) = chain.split_last() else { return Err(XzError::Options) };
    if last.id != FILTER_LZMA2 || init.iter().any(|f| is_lzma(f.id)) {
        return Err(XzError::Options);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
