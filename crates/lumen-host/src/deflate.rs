//! DEFLATE (RFC 1951) with zlib (RFC 1950) and gzip (RFC 1952) framing — a from-scratch, std-only
//! codec shared by the web `CompressionStream`/`DecompressionStream` and `node:zlib`. No external
//! crates: inflate handles stored/fixed/dynamic Huffman blocks; deflate emits fixed-Huffman blocks
//! with greedy LZ77 matching (real compression, not stored-only). Checksums: Adler-32 (zlib) and
//! CRC-32 (gzip).

// ---- checksums --------------------------------------------------------------------------------

pub fn adler32(data: &[u8]) -> u32 {
    adler32_from(1, data)
}

/// Adler-32 continued from a prior checksum (`1` to start fresh), for streamed input.
pub fn adler32_from(adler: u32, data: &[u8]) -> u32 {
    let (mut a, mut b) = (adler & 0xffff, adler >> 16);
    // 5552 bytes is the most that can be summed before `b` could overflow a u32 (zlib's NMAX).
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    crc32_from(0, data)
}

/// CRC-32 continued from a prior checksum `seed` (0 to start fresh) — the form `node:zlib.crc32`
/// exposes so callers can chain checksums across chunks.
pub fn crc32_from(seed: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(crc32_table);
    let mut crc = seed ^ 0xffff_ffff;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

// ---- bit reader (LSB-first, per DEFLATE) ------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bit_buf: 0,
            bit_cnt: 0,
        }
    }
    fn bit(&mut self) -> Result<u32, String> {
        if self.bit_cnt == 0 {
            if self.pos >= self.data.len() {
                return Err("inflate: unexpected end of input".into());
            }
            self.bit_buf = self.data[self.pos] as u32;
            self.pos += 1;
            self.bit_cnt = 8;
        }
        let b = self.bit_buf & 1;
        self.bit_buf >>= 1;
        self.bit_cnt -= 1;
        Ok(b)
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }
    fn align_to_byte(&mut self) {
        self.bit_buf = 0;
        self.bit_cnt = 0;
    }
}

// ---- Huffman decoding -------------------------------------------------------------------------

/// Canonical Huffman decode table built from per-symbol code lengths.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &len in lengths {
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for i in 1..16 {
            offsets[i] = offsets[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }
    fn decode(&self, reader: &mut BitReader) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= reader.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("inflate: invalid Huffman code".into())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_huffman() -> (Huffman, Huffman) {
    let mut lit_lengths = [0u8; 288];
    for (i, len) in lit_lengths.iter_mut().enumerate() {
        *len = if i < 144 {
            8
        } else if i < 256 {
            9
        } else if i < 280 {
            7
        } else {
            8
        };
    }
    let dist_lengths = [5u8; 30];
    (Huffman::new(&lit_lengths), Huffman::new(&dist_lengths))
}

fn inflate_block(
    reader: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(reader)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()), // end of block
            257..=285 => {
                let i = (sym - 257) as usize;
                let length =
                    LENGTH_BASE[i] as usize + reader.bits(LENGTH_EXTRA[i] as u32)? as usize;
                let dsym = dist.decode(reader)? as usize;
                if dsym >= 30 {
                    return Err("inflate: invalid distance symbol".into());
                }
                let distance =
                    DIST_BASE[dsym] as usize + reader.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if distance > out.len() {
                    return Err("inflate: distance too far back".into());
                }
                let start = out.len() - distance;
                for k in 0..length {
                    out.push(out[start + k]);
                }
            }
            _ => return Err("inflate: invalid literal/length symbol".into()),
        }
    }
}

/// Decode raw DEFLATE (no zlib/gzip wrapper).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::new();
    while !inflate_one_block(&mut reader, &mut out)? {}
    Ok(out)
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn read_dynamic_tables(reader: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;

    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CODE_LENGTH_ORDER[i]] = reader.bits(3)? as u8;
    }
    let cl_huffman = Huffman::new(&cl_lengths);

    let mut lengths = Vec::with_capacity(hlit + hdist);
    while lengths.len() < hlit + hdist {
        let sym = cl_huffman.decode(reader)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths
                    .last()
                    .ok_or("inflate: repeat with no previous length")?;
                for _ in 0..(reader.bits(2)? + 3) {
                    lengths.push(prev);
                }
            }
            17 => {
                let n = reader.bits(3)? as usize + 3;
                lengths.resize(lengths.len() + n, 0);
            }
            18 => {
                let n = reader.bits(7)? as usize + 11;
                lengths.resize(lengths.len() + n, 0);
            }
            _ => return Err("inflate: invalid code-length symbol".into()),
        }
    }
    if lengths.len() > hlit + hdist {
        return Err("inflate: code-length overrun".into());
    }
    let (lit_lengths, dist_lengths) = lengths.split_at(hlit);
    Ok((Huffman::new(lit_lengths), Huffman::new(dist_lengths)))
}

// ---- deflate encoder (fixed Huffman + greedy LZ77) --------------------------------------------

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bit_buf: u32,
    bit_cnt: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            out: Vec::new(),
            bit_buf: 0,
            bit_cnt: 0,
        }
    }
    fn write(&mut self, value: u32, n: u32) {
        self.bit_buf |= value << self.bit_cnt;
        self.bit_cnt += n;
        while self.bit_cnt >= 8 {
            self.out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf >>= 8;
            self.bit_cnt -= 8;
        }
    }
    /// Huffman codes are written MSB-first (bit-reversed relative to the LSB bit order).
    fn write_code(&mut self, code: u32, n: u32) {
        let mut reversed = 0;
        for i in 0..n {
            reversed |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.write(reversed, n);
    }
    /// Pad to the next byte boundary (the start of a stored block's LEN field).
    fn align(&mut self) {
        if self.bit_cnt > 0 {
            self.out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf = 0;
            self.bit_cnt = 0;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bit_cnt > 0 {
            self.out.push((self.bit_buf & 0xff) as u8);
        }
        self.out
    }
}

/// Fixed-Huffman literal/length code for a symbol (0..=287) — code value and bit length.
fn fixed_lit_code(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + sym as u32, 8),
        144..=255 => (0x190 + (sym as u32 - 144), 9),
        256..=279 => (sym as u32 - 256, 7),
        _ => (0xc0 + (sym as u32 - 280), 8),
    }
}

fn length_symbol(length: usize) -> (u16, u32, u32) {
    for i in (0..29).rev() {
        if length >= LENGTH_BASE[i] as usize {
            let extra = length - LENGTH_BASE[i] as usize;
            return (257 + i as u16, extra as u32, LENGTH_EXTRA[i] as u32);
        }
    }
    (257, 0, 0)
}

fn dist_symbol(distance: usize) -> (u16, u32, u32) {
    for i in (0..30).rev() {
        if distance >= DIST_BASE[i] as usize {
            let extra = distance - DIST_BASE[i] as usize;
            return (i as u16, extra as u32, DIST_EXTRA[i] as u32);
        }
    }
    (0, 0, 0)
}

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

fn hash3(data: &[u8], i: usize) -> usize {
    let v = (data[i] as usize) << 16 | (data[i + 1] as usize) << 8 | data[i + 2] as usize;
    (v.wrapping_mul(2654435761)) >> (32 - HASH_BITS) & (HASH_SIZE - 1)
}

const WINDOW_SIZE: usize = 32 * 1024;

/// Encode raw DEFLATE: one fixed-Huffman block with greedy LZ77 (hash-chain match finder).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    deflate_block(&mut w, &[], data, true);
    w.finish()
}

/// Append one fixed-Huffman block encoding `data` to `w`. Matches may reach back into
/// `history` (the bytes the decoder already holds in its window), which is what lets a stream
/// keep compressing across flushes with context takeover.
fn deflate_block(w: &mut BitWriter, history: &[u8], data: &[u8], final_block: bool) {
    w.write(final_block as u32, 1); // BFINAL
    w.write(1, 2); // BTYPE = 01 (fixed Huffman)

    let emit_literal = |w: &mut BitWriter, byte: u8| {
        let (code, len) = fixed_lit_code(byte as u16);
        w.write_code(code, len);
    };

    let history = &history[history.len().saturating_sub(WINDOW_SIZE)..];
    let buf: Vec<u8> = [history, data].concat();
    let n = buf.len();
    let start = history.len();
    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut prev = vec![usize::MAX; n.max(1)];
    let insert = |head: &mut Vec<usize>, prev: &mut Vec<usize>, j: usize| {
        let h = hash3(&buf, j);
        prev[j] = head[h];
        head[h] = j;
    };
    for j in 0..start.saturating_sub(MIN_MATCH - 1) {
        insert(&mut head, &mut prev, j);
    }
    let mut i = start;
    while i < n {
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + MIN_MATCH <= n {
            let h = hash3(&buf, i);
            let mut cand = head[h];
            let mut chain = 0;
            while cand != usize::MAX && chain < 128 && i - cand <= WINDOW_SIZE {
                let max_len = (n - i).min(MAX_MATCH);
                let mut len = 0;
                while len < max_len && buf[cand + len] == buf[i + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = i - cand;
                    if len >= max_len {
                        break;
                    }
                }
                cand = prev[cand];
                chain += 1;
            }
            prev[i] = head[h];
            head[h] = i;
        }

        if best_len >= MIN_MATCH {
            let (lsym, lextra, lbits) = length_symbol(best_len);
            let (lcode, lcodelen) = fixed_lit_code(lsym);
            w.write_code(lcode, lcodelen);
            if lbits > 0 {
                w.write(lextra, lbits);
            }
            let (dsym, dextra, dbits) = dist_symbol(best_dist);
            w.write_code(dsym as u32, 5);
            if dbits > 0 {
                w.write(dextra, dbits);
            }
            // Insert hash entries for the bytes the match covers (skip the first, already inserted).
            let end = i + best_len;
            let mut j = i + 1;
            while j < end && j + MIN_MATCH <= n {
                insert(&mut head, &mut prev, j);
                j += 1;
            }
            i = end;
        } else {
            emit_literal(w, buf[i]);
            i += 1;
        }
    }
    // End-of-block symbol (256).
    let (code, len) = fixed_lit_code(256);
    w.write_code(code, len);
}

// ---- streaming (node:zlib Deflate/Inflate objects) --------------------------------------------

/// How a [`Deflater::flush`] ends the data written so far — the subset of zlib's flush modes
/// that produce distinct output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flush {
    /// Buffer only; nothing is emitted.
    None,
    /// Close the current block and append an empty stored block, so everything written so far
    /// is decodable and the output ends on a byte boundary (`Z_SYNC_FLUSH`; zlib's
    /// `Z_PARTIAL_FLUSH` / `Z_BLOCK` are served the same way — also decodable, just 4 bytes more).
    Sync,
    /// [`Flush::Sync`], then forget the window: later output never references earlier bytes, so
    /// a decoder may start from here (`Z_FULL_FLUSH`).
    Full,
    /// Emit the final block (`Z_FINISH`).
    Finish,
}

impl Flush {
    /// zlib's numeric flush constant (`Z_NO_FLUSH` = 0 … `Z_FINISH` = 4, `Z_BLOCK` = 5; Brotli's
    /// and zstd's use other numbers and never reach here).
    pub fn from_zlib(mode: u32) -> Option<Flush> {
        Some(match mode {
            0 => Flush::None,
            1 | 2 | 5 => Flush::Sync,
            3 => Flush::Full,
            4 => Flush::Finish,
            _ => return None,
        })
    }
}

/// Input a [`Deflater`] holds before it emits a block on its own (between explicit flushes), so
/// a long stream produces output as it goes rather than all at its end.
const STREAM_BLOCK: usize = 64 * 1024;

/// Incremental raw-DEFLATE encoder: bytes accumulate across `write`s and are emitted as
/// fixed-Huffman blocks — whenever [`STREAM_BLOCK`] bytes are pending, and at every flush. The
/// bit stream carries across calls (a block need not end on a byte boundary), and the last 32K
/// of input stays as the match window, so a stream flushed per message (websocket
/// permessage-deflate) still compresses across messages.
#[derive(Default)]
pub struct Deflater {
    history: Vec<u8>,
    pending: Vec<u8>,
    bits: BitWriter,
    finished: bool,
}

impl Deflater {
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer `data`; nothing is encoded until the next flush.
    pub fn write(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
    }

    /// Buffer `data`, encoding full [`STREAM_BLOCK`]s as they fill; returns the output bytes
    /// completed so far (a trailing partial byte stays behind for the next block).
    pub fn write_eager(&mut self, data: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(data);
        if self.finished || self.pending.len() < STREAM_BLOCK {
            return Vec::new();
        }
        self.emit_block(false);
        std::mem::take(&mut self.bits.out)
    }

    fn emit_block(&mut self, final_block: bool) {
        let pending = std::mem::take(&mut self.pending);
        deflate_block(&mut self.bits, &self.history, &pending, final_block);
        self.history.extend_from_slice(&pending);
        if self.history.len() > WINDOW_SIZE {
            self.history.drain(..self.history.len() - WINDOW_SIZE);
        }
    }

    pub fn flush(&mut self, mode: Flush) -> Result<Vec<u8>, String> {
        if self.finished {
            return Err("deflate: stream already finished".into());
        }
        match mode {
            Flush::None => return Ok(std::mem::take(&mut self.bits.out)),
            Flush::Finish => {
                self.emit_block(true);
                self.bits.align();
                self.finished = true;
            }
            Flush::Sync | Flush::Full => {
                if !self.pending.is_empty() {
                    self.emit_block(false);
                }
                // Empty stored block: BFINAL=0, BTYPE=00, pad to a byte, LEN=0, NLEN=0xffff.
                self.bits.write(0, 3);
                self.bits.align();
                self.bits.out.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
                if mode == Flush::Full {
                    self.history.clear();
                }
            }
        }
        Ok(std::mem::take(&mut self.bits.out))
    }

    /// Forget the window (`zlib.reset()`): the next block cannot reference earlier output.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The container around a DEFLATE stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framing {
    /// Bare DEFLATE (`DeflateRaw` / `InflateRaw`).
    Raw,
    /// RFC 1950: a 2-byte header and an Adler-32 trailer (`Deflate` / `Inflate`).
    Zlib,
    /// RFC 1952: a 10-byte header and a CRC-32 + length trailer (`Gzip` / `Gunzip`).
    Gzip,
    /// Decoding only: gzip or zlib, whichever the header is (`Unzip`).
    Auto,
}

/// Incremental compressor with zlib/gzip framing over a [`Deflater`]: the header goes out with
/// the first output, the checksum runs over the input as it arrives, and the trailer follows the
/// final block.
pub struct FramedDeflater {
    framing: Framing,
    inner: Deflater,
    header_sent: bool,
    check: u32,
    size: u32,
}

impl FramedDeflater {
    pub fn new(framing: Framing) -> Self {
        FramedDeflater {
            framing,
            inner: Deflater::new(),
            header_sent: false,
            check: if framing == Framing::Zlib { 1 } else { 0 },
            size: 0,
        }
    }

    fn frame(&mut self, body: Vec<u8>) -> Vec<u8> {
        if self.header_sent || body.is_empty() {
            return body;
        }
        self.header_sent = true;
        let mut out = match self.framing {
            // CMF/FLG: deflate, 32K window, default level.
            Framing::Zlib => vec![0x78, 0x9c],
            // magic, method, flags, mtime, xfl, os (unknown) — what Node's zlib writes.
            Framing::Gzip => vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0x0a],
            Framing::Raw | Framing::Auto => Vec::new(),
        };
        out.extend_from_slice(&body);
        out
    }

    /// Feed input; returns whatever output is complete (possibly nothing).
    pub fn write(&mut self, data: &[u8]) -> Vec<u8> {
        match self.framing {
            Framing::Zlib => self.check = adler32_from(self.check, data),
            Framing::Gzip => self.check = crc32_from(self.check, data),
            Framing::Raw | Framing::Auto => {}
        }
        self.size = self.size.wrapping_add(data.len() as u32);
        let body = self.inner.write_eager(data);
        self.frame(body)
    }

    pub fn flush(&mut self, mode: Flush) -> Result<Vec<u8>, String> {
        let body = self.inner.flush(mode)?;
        let mut out = self.frame(body);
        if mode == Flush::Finish {
            match self.framing {
                Framing::Zlib => out.extend_from_slice(&self.check.to_be_bytes()),
                Framing::Gzip => {
                    out.extend_from_slice(&self.check.to_le_bytes());
                    out.extend_from_slice(&self.size.to_le_bytes());
                }
                Framing::Raw | Framing::Auto => {}
            }
        }
        Ok(out)
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.framing);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrameState {
    Header,
    Body,
    Trailer,
    Done,
}

/// Incremental decompressor with zlib/gzip framing over an [`Inflater`]. Input may be split
/// anywhere, including inside the header or trailer; the trailer's checksum (and gzip's length)
/// is verified. Concatenated gzip members decode as one stream, as with Node's `Gunzip`; other
/// bytes after the end are ignored.
pub struct FramedInflater {
    framing: Framing,
    /// The framing the header turned out to be (for [`Framing::Auto`]).
    actual: Framing,
    state: FrameState,
    buf: Vec<u8>,
    inner: Inflater,
    check: u32,
    size: u32,
    /// Bytes that were not another gzip member followed the end.
    garbage: bool,
}

impl FramedInflater {
    pub fn new(framing: Framing) -> Self {
        FramedInflater {
            framing,
            actual: framing,
            state: if framing == Framing::Raw {
                FrameState::Body
            } else {
                FrameState::Header
            },
            buf: Vec::new(),
            inner: Inflater::new(),
            check: 0,
            size: 0,
            garbage: false,
        }
    }

    /// Whether the stream (the last gzip member) ended, trailer included.
    pub fn finished(&self) -> bool {
        self.state == FrameState::Done
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.framing);
    }

    /// The header's length once `buf` holds all of it; `Ok(None)` while it is incomplete.
    fn parse_header(&mut self) -> Result<Option<usize>, String> {
        let b = &self.buf;
        if self.actual == Framing::Auto {
            if b.is_empty() {
                return Ok(None);
            }
            self.actual = if b[0] == 0x1f {
                Framing::Gzip
            } else {
                Framing::Zlib
            };
        }
        match self.actual {
            Framing::Zlib => {
                if b.len() < 2 {
                    return Ok(None);
                }
                if b[0] & 0x0f != 8 || ((b[0] as u32) << 8 | b[1] as u32) % 31 != 0 {
                    return Err("incorrect header check".into());
                }
                if b[1] & 0x20 != 0 {
                    return Err("Missing dictionary".into());
                }
                Ok(Some(2))
            }
            Framing::Gzip => {
                if b.len() < 10 {
                    if b.first().is_some_and(|&x| x != 0x1f)
                        || b.get(1).is_some_and(|&x| x != 0x8b)
                    {
                        return Err("incorrect header check".into());
                    }
                    return Ok(None);
                }
                if b[0] != 0x1f || b[1] != 0x8b {
                    return Err("incorrect header check".into());
                }
                if b[2] != 8 {
                    return Err("unknown compression method".into());
                }
                let flags = b[3];
                let mut pos = 10;
                if flags & 0x04 != 0 {
                    if b.len() < pos + 2 {
                        return Ok(None);
                    }
                    pos += 2 + (b[pos] as usize | (b[pos + 1] as usize) << 8);
                }
                for bit in [0x08, 0x10] {
                    // FNAME / FCOMMENT: NUL-terminated.
                    if flags & bit != 0 {
                        match b.get(pos..).and_then(|t| t.iter().position(|&c| c == 0)) {
                            Some(n) => pos += n + 1,
                            None => return Ok(None),
                        }
                    }
                }
                if flags & 0x02 != 0 {
                    pos += 2; // FHCRC
                }
                Ok((b.len() >= pos).then_some(pos))
            }
            Framing::Raw | Framing::Auto => Ok(Some(0)),
        }
    }

    /// Feed input; returns the decompressed bytes it completed.
    pub fn write(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut input = data.to_vec();
        loop {
            match self.state {
                FrameState::Header => {
                    self.buf.extend_from_slice(&input);
                    let Some(n) = self.parse_header()? else {
                        return Ok(out);
                    };
                    input = self.buf.split_off(n);
                    self.buf.clear();
                    self.check = if self.actual == Framing::Zlib { 1 } else { 0 };
                    self.size = 0;
                    self.state = FrameState::Body;
                }
                FrameState::Body => {
                    let produced = self.inner.write(&input)?;
                    match self.actual {
                        Framing::Zlib => self.check = adler32_from(self.check, &produced),
                        Framing::Gzip => self.check = crc32_from(self.check, &produced),
                        Framing::Raw | Framing::Auto => {}
                    }
                    self.size = self.size.wrapping_add(produced.len() as u32);
                    out.extend_from_slice(&produced);
                    if !self.inner.finished() {
                        return Ok(out);
                    }
                    input = self.inner.take_remaining();
                    self.state = FrameState::Trailer;
                }
                FrameState::Trailer => {
                    self.buf.extend_from_slice(&input);
                    let need = match self.actual {
                        Framing::Zlib => 4,
                        Framing::Gzip => 8,
                        Framing::Raw | Framing::Auto => 0,
                    };
                    if self.buf.len() < need {
                        return Ok(out);
                    }
                    let t = &self.buf[..need];
                    match self.actual {
                        Framing::Zlib => {
                            if u32::from_be_bytes([t[0], t[1], t[2], t[3]]) != self.check {
                                return Err("incorrect data check".into());
                            }
                        }
                        Framing::Gzip => {
                            if u32::from_le_bytes([t[0], t[1], t[2], t[3]]) != self.check {
                                return Err("incorrect data check".into());
                            }
                            if u32::from_le_bytes([t[4], t[5], t[6], t[7]]) != self.size {
                                return Err("incorrect length check".into());
                            }
                        }
                        Framing::Raw | Framing::Auto => {}
                    }
                    input = self.buf.split_off(need);
                    self.buf.clear();
                    self.state = FrameState::Done;
                }
                FrameState::Done => {
                    // Another gzip member follows: decode it into the same output. Anything
                    // else after the end is ignored, and so is everything after that.
                    let Some(&first) = input.first() else {
                        return Ok(out);
                    };
                    if self.actual != Framing::Gzip || first != 0x1f || self.garbage {
                        self.garbage = true;
                        return Ok(out);
                    }
                    self.inner = Inflater::new();
                    self.state = FrameState::Header;
                }
            }
        }
    }
}

/// Incremental raw-DEFLATE decoder. Input may arrive at any boundary: a block that runs past the
/// bytes seen so far is retried from its start when more input arrives, and the last 32K of
/// output is kept so back-references across writes (and across messages, with context takeover)
/// resolve.
#[derive(Default)]
pub struct Inflater {
    window: Vec<u8>,
    pending: Vec<u8>,
    bit_buf: u32,
    bit_cnt: u32,
    finished: bool,
}

impl Inflater {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes and return whatever became decodable. Once the final block has been decoded,
    /// `finished()` reports true and further input is ignored.
    pub fn write(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.pending.extend_from_slice(data);
        let mut out = std::mem::take(&mut self.window);
        let window_len = out.len();
        let mut reader = BitReader::new(&self.pending);
        reader.bit_buf = self.bit_buf;
        reader.bit_cnt = self.bit_cnt;
        loop {
            let checkpoint = (reader.pos, reader.bit_buf, reader.bit_cnt, out.len());
            match inflate_one_block(&mut reader, &mut out) {
                Ok(final_block) => {
                    if final_block {
                        self.finished = true;
                        break;
                    }
                }
                Err(e) if e.contains("unexpected end of input")
                    || e.contains("truncated")
                    || e.contains("overruns input") =>
                {
                    (reader.pos, reader.bit_buf, reader.bit_cnt) =
                        (checkpoint.0, checkpoint.1, checkpoint.2);
                    out.truncate(checkpoint.3);
                    break;
                }
                Err(e) => {
                    self.window = out;
                    return Err(e);
                }
            }
        }
        let (consumed, bit_buf, bit_cnt) = (reader.pos, reader.bit_buf, reader.bit_cnt);
        self.pending.drain(..consumed);
        self.bit_buf = bit_buf;
        self.bit_cnt = bit_cnt;
        let produced = out.split_off(window_len);
        out.extend_from_slice(&produced);
        if out.len() > WINDOW_SIZE {
            out.drain(..out.len() - WINDOW_SIZE);
        }
        self.window = out;
        Ok(produced)
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// After the final block: the input bytes that followed it (a container's trailer).
    pub fn take_remaining(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Decode one block from `reader` into `out`; returns whether it was the final block.
fn inflate_one_block(reader: &mut BitReader, out: &mut Vec<u8>) -> Result<bool, String> {
    let final_block = reader.bit()?;
    let btype = reader.bits(2)?;
    let data = reader.data;
    match btype {
        0 => {
            reader.align_to_byte();
            if reader.pos + 4 > data.len() {
                return Err("inflate: truncated stored block".into());
            }
            let len = data[reader.pos] as usize | ((data[reader.pos + 1] as usize) << 8);
            reader.pos += 4; // LEN + NLEN
            if reader.pos + len > data.len() {
                return Err("inflate: stored block overruns input".into());
            }
            out.extend_from_slice(&data[reader.pos..reader.pos + len]);
            reader.pos += len;
        }
        1 => {
            let (lit, dist) = fixed_huffman();
            inflate_block(reader, out, &lit, &dist)?;
        }
        2 => {
            let (lit, dist) = read_dynamic_tables(reader)?;
            inflate_block(reader, out, &lit, &dist)?;
        }
        _ => return Err("inflate: reserved block type".into()),
    }
    Ok(final_block == 1)
}

// ---- zlib / gzip framing ----------------------------------------------------------------------

pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x9c]; // CMF/FLG (deflate, default window, default level)
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 {
        return Err("zlib: input too short".into());
    }
    if data[0] & 0x0f != 8 {
        return Err("zlib: unsupported compression method".into());
    }
    let out = inflate(&data[2..])?;
    Ok(out)
}

pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]; // magic, method, flags, mtime, xfl, os
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return Err("gzip: bad magic".into());
    }
    if data[2] != 8 {
        return Err("gzip: unsupported compression method".into());
    }
    let flags = data[3];
    let mut pos = 10;
    if flags & 0x04 != 0 {
        // FEXTRA
        if pos + 2 > data.len() {
            return Err("gzip: truncated extra field".into());
        }
        let xlen = data[pos] as usize | ((data[pos + 1] as usize) << 8);
        pos += 2 + xlen;
    }
    if flags & 0x08 != 0 {
        // FNAME (NUL-terminated)
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flags & 0x10 != 0 {
        // FCOMMENT
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flags & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    if pos + 8 > data.len() {
        return Err("gzip: truncated".into());
    }
    inflate(&data[pos..data.len() - 8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        assert_eq!(inflate(&deflate(data)).unwrap(), data, "raw deflate");
        assert_eq!(zlib_decompress(&zlib_compress(data)).unwrap(), data, "zlib");
        assert_eq!(gzip_decompress(&gzip_compress(data)).unwrap(), data, "gzip");
    }

    #[test]
    fn roundtrips() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"hello, hello, hello world!");
        roundtrip(&[0u8; 1000]); // long run — exercises back-references
        let repetitive: Vec<u8> = (0..5000).map(|i| (i % 7) as u8).collect();
        roundtrip(&repetitive);
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(50);
        roundtrip(text.as_bytes());
    }

    #[test]
    fn compresses_repetitive_input() {
        let data = "abcabcabcabc".repeat(100);
        let compressed = deflate(data.as_bytes());
        assert!(
            compressed.len() < data.len() / 2,
            "expected real compression"
        );
    }

    #[test]
    fn streaming_roundtrip_with_context_takeover() {
        let messages: Vec<&[u8]> = vec![b"hello world", b"hello world again", b"", b"and hello world once more"];
        let mut enc = Deflater::new();
        let mut dec = Inflater::new();
        for (i, m) in messages.iter().enumerate() {
            enc.write(m);
            let frame = enc.flush(Flush::Sync).unwrap();
            assert_eq!(&frame[frame.len() - 4..], &[0, 0, 0xff, 0xff], "sync flush trailer");
            // Feed byte by byte: every block boundary must survive an arbitrary split.
            let mut got = Vec::new();
            for b in &frame {
                got.extend(dec.write(std::slice::from_ref(b)).unwrap());
            }
            assert_eq!(&got, m, "message {i}");
            assert!(!dec.finished());
        }
        assert!(!enc.flush(Flush::Finish).unwrap().is_empty());
        // The second message referenced the first: with the window, it is smaller than alone.
        let mut lone = Deflater::new();
        lone.write(messages[1]);
        let lone_frame = lone.flush(Flush::Sync).unwrap();
        let mut ctx = Deflater::new();
        ctx.write(messages[0]);
        ctx.flush(Flush::Sync).unwrap();
        ctx.write(messages[1]);
        assert!(ctx.flush(Flush::Sync).unwrap().len() < lone_frame.len());
    }

    #[test]
    fn streaming_inflate_matches_one_shot_and_finishes() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(2000);
        let compressed = deflate(text.as_bytes());
        let mut dec = Inflater::new();
        let mut got = Vec::new();
        for chunk in compressed.chunks(7) {
            got.extend(dec.write(chunk).unwrap());
        }
        assert!(dec.finished());
        assert_eq!(got, text.as_bytes());
        assert!(dec.write(b"ignored").unwrap().is_empty());
        let mut enc = Deflater::new();
        enc.write(text.as_bytes());
        let out = enc.flush(Flush::Finish).unwrap();
        assert_eq!(inflate(&out).unwrap(), text.as_bytes());
        assert!(enc.flush(Flush::Sync).is_err());
    }

    #[test]
    fn framed_streams_flush_partially_and_roundtrip() {
        for framing in [Framing::Gzip, Framing::Zlib, Framing::Raw] {
            let mut enc = FramedDeflater::new(framing);
            let mut dec = FramedInflater::new(if framing == Framing::Raw {
                Framing::Raw
            } else {
                Framing::Auto
            });
            for (i, m) in [&b"abc"[..], b"def", b"", b"ghi"].iter().enumerate() {
                let mut frame = enc.write(m);
                frame.extend(enc.flush(if i == 1 { Flush::Full } else { Flush::Sync }).unwrap());
                // Every flush makes exactly what was written so far decodable, split anywhere.
                let mut got = Vec::new();
                for b in &frame {
                    got.extend(dec.write(std::slice::from_ref(b)).unwrap());
                }
                assert_eq!(&got, m, "{framing:?} message {i}");
            }
            // Larger than one eager block: output appears before any flush.
            let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
            let early = enc.write(&big);
            assert!(!early.is_empty(), "{framing:?}: no eager output");
            let mut stream = early;
            stream.extend(enc.flush(Flush::Finish).unwrap());
            let got = dec.write(&stream).unwrap();
            assert_eq!(got, big, "{framing:?} big");
            if framing != Framing::Raw {
                assert!(dec.finished());
            }
        }
        // A framed stream decodes with the one-shot decoders too.
        let mut enc = FramedDeflater::new(Framing::Gzip);
        let mut all = enc.write(b"hello ");
        all.extend(enc.flush(Flush::Sync).unwrap());
        all.extend(enc.write(b"world"));
        all.extend(enc.flush(Flush::Finish).unwrap());
        assert_eq!(gzip_decompress(&all).unwrap(), b"hello world");
        // Concatenated gzip members, fed a byte at a time, decode as one stream.
        let two = [gzip_compress(b"abc"), gzip_compress(b"def")].concat();
        let mut dec = FramedInflater::new(Framing::Gzip);
        let mut got = Vec::new();
        for b in &two {
            got.extend(dec.write(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, b"abcdef");
        // A corrupted checksum is reported.
        let mut bad = zlib_compress(b"payload");
        let n = bad.len();
        bad[n - 1] ^= 1;
        let err = FramedInflater::new(Framing::Zlib).write(&bad).unwrap_err();
        assert_eq!(err, "incorrect data check");
        assert_eq!(adler32_from(adler32(b"Wiki"), b"pedia"), adler32(b"Wikipedia"));
    }

    #[test]
    fn streaming_inflate_rejects_garbage() {
        let mut dec = Inflater::new();
        assert!(dec.write(&[0x07, 0xff, 0xff]).is_err()); // BTYPE = 11 is reserved
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }
}
