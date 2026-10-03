//! The LZMA1 and LZMA2 decoders, resumable at any byte of input.
//!
//! LZMA2 chunks carry their packed size, so a chunk is buffered whole and then decoded in one go.
//! LZMA1 has no framing: each symbol is decoded from the bytes at hand, and when they run out in
//! the middle of one, the probability updates are rolled back from a log and the bytes are kept
//! until more input arrives.

use super::XzError;

pub(super) enum Fail {
    Need,
    Data,
}

pub(super) trait ByteSrc {
    fn next(&mut self) -> Option<u8>;
}

struct SliceSrc<'a> {
    data: &'a [u8],
    pos: usize,
}

impl ByteSrc for SliceSrc<'_> {
    fn next(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
}

/// Reads first from the bytes kept from a failed attempt, then from the caller's input (which it
/// also appends to the kept bytes, so a retry sees them again).
struct Lazy<'a> {
    kept: &'a mut Vec<u8>,
    at: usize,
    data: &'a [u8],
    taken: &'a mut usize,
}

impl ByteSrc for Lazy<'_> {
    fn next(&mut self) -> Option<u8> {
        if self.at < self.kept.len() {
            let b = self.kept[self.at];
            self.at += 1;
            return Some(b);
        }
        let b = *self.data.get(*self.taken)?;
        *self.taken += 1;
        self.kept.push(b);
        self.at += 1;
        Some(b)
    }
}

const IS_MATCH: usize = 0;
const IS_REP: usize = IS_MATCH + 12 * 16;
const IS_REP_G0: usize = IS_REP + 12;
const IS_REP_G1: usize = IS_REP_G0 + 12;
const IS_REP_G2: usize = IS_REP_G1 + 12;
const IS_REP0_LONG: usize = IS_REP_G2 + 12;
const POS_SLOT: usize = IS_REP0_LONG + 12 * 16;
const SPEC_POS: usize = POS_SLOT + 4 * 64;
const ALIGN: usize = SPEC_POS + 115;
const LEN_CODER: usize = ALIGN + 16;
const LEN_SIZE: usize = 2 + 2 * 16 * 8 + 256;
const REP_LEN_CODER: usize = LEN_CODER + LEN_SIZE;
const LITERAL: usize = REP_LEN_CODER + LEN_SIZE;

const PROB_INIT: u16 = 1024;
const MATCH_MIN: usize = 2;

/// The sliding window of decoded bytes.
pub(super) struct Dict {
    buf: Vec<u8>,
    cap: usize,
    pos: usize,
    len: usize,
    total: u64,
}

impl Dict {
    fn new(cap: usize) -> Dict {
        Dict { buf: Vec::new(), cap: cap.max(4096), pos: 0, len: 0, total: 0 }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.len = 0;
        self.total = 0;
    }

    fn put(&mut self, b: u8) {
        if self.buf.len() < self.cap {
            self.buf.push(b);
            self.pos = self.buf.len() % self.cap;
        } else {
            self.buf[self.pos] = b;
            self.pos = (self.pos + 1) % self.cap;
        }
        self.len = (self.len + 1).min(self.cap);
        self.total += 1;
    }

    /// The byte `dist` positions back (1 is the last one written).
    fn get(&self, dist: usize) -> u8 {
        let n = self.buf.len();
        let at = if dist <= self.pos { self.pos - dist } else { n - (dist - self.pos) };
        self.buf[at]
    }

    fn prev(&self) -> u8 {
        if self.len == 0 {
            0
        } else {
            self.get(1)
        }
    }
}

enum Symbol {
    Literal(u8),
    Match { dist: u32, len: usize },
    ShortRep,
    End,
}

#[derive(Clone, Copy)]
struct Saved {
    range: u32,
    code: u32,
    state: usize,
    reps: [u32; 4],
}

pub(super) struct Lzma {
    lc: u32,
    lp: u32,
    pb: u32,
    probs: Vec<u16>,
    state: usize,
    reps: [u32; 4],
    range: u32,
    code: u32,
    pub(super) dict: Dict,
    undo: Vec<(u32, u16)>,
    logging: bool,
}

impl Lzma {
    pub(super) fn new(dict_size: u32) -> Lzma {
        Lzma {
            lc: 0,
            lp: 0,
            pb: 0,
            probs: Vec::new(),
            state: 0,
            reps: [0; 4],
            range: 0,
            code: 0,
            dict: Dict::new(dict_size as usize),
            undo: Vec::new(),
            logging: false,
        }
    }

    pub(super) fn set_props(&mut self, lc: u32, lp: u32, pb: u32) {
        self.lc = lc;
        self.lp = lp;
        self.pb = pb;
        self.reset_state();
    }

    pub(super) fn reset_state(&mut self) {
        self.probs.clear();
        self.probs.resize(LITERAL + (0x300usize << (self.lc + self.lp)), PROB_INIT);
        self.state = 0;
        self.reps = [0; 4];
    }

    fn save(&self) -> Saved {
        Saved { range: self.range, code: self.code, state: self.state, reps: self.reps }
    }

    fn restore(&mut self, s: Saved) {
        self.range = s.range;
        self.code = s.code;
        self.state = s.state;
        self.reps = s.reps;
        while let Some((i, old)) = self.undo.pop() {
            self.probs[i as usize] = old;
        }
    }

    fn init_rc<S: ByteSrc>(&mut self, src: &mut S) -> Result<(), Fail> {
        let mut bytes = [0u8; 5];
        for b in &mut bytes {
            *b = src.next().ok_or(Fail::Need)?;
        }
        if bytes[0] != 0 {
            return Err(Fail::Data);
        }
        self.range = u32::MAX;
        self.code = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        Ok(())
    }

    fn normalize<S: ByteSrc>(&mut self, src: &mut S) -> Result<(), Fail> {
        if self.range < 1 << 24 {
            let b = src.next().ok_or(Fail::Need)?;
            self.range <<= 8;
            self.code = (self.code << 8) | u32::from(b);
        }
        Ok(())
    }

    fn bit<S: ByteSrc>(&mut self, i: usize, src: &mut S) -> Result<u32, Fail> {
        self.normalize(src)?;
        let prob = self.probs[i];
        if self.logging {
            self.undo.push((i as u32, prob));
        }
        let bound = (self.range >> 11) * u32::from(prob);
        if self.code < bound {
            self.range = bound;
            self.probs[i] = prob + ((2048 - prob) >> 5);
            Ok(0)
        } else {
            self.range -= bound;
            self.code -= bound;
            self.probs[i] = prob - (prob >> 5);
            Ok(1)
        }
    }

    fn tree<S: ByteSrc>(&mut self, base: usize, bits: u32, src: &mut S) -> Result<u32, Fail> {
        let mut m = 1u32;
        for _ in 0..bits {
            m = (m << 1) | self.bit(base + m as usize, src)?;
        }
        Ok(m - (1 << bits))
    }

    fn reverse_tree<S: ByteSrc>(&mut self, base: usize, bits: u32, src: &mut S) -> Result<u32, Fail> {
        let mut m = 1u32;
        let mut sym = 0u32;
        for i in 0..bits {
            let b = self.bit(base + m as usize, src)?;
            m = (m << 1) | b;
            sym |= b << i;
        }
        Ok(sym)
    }

    fn direct<S: ByteSrc>(&mut self, bits: u32, src: &mut S) -> Result<u32, Fail> {
        let mut v = 0u32;
        for _ in 0..bits {
            self.normalize(src)?;
            self.range >>= 1;
            let b = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            v = (v << 1) | b;
        }
        Ok(v)
    }

    fn length<S: ByteSrc>(&mut self, base: usize, pos_state: usize, src: &mut S) -> Result<usize, Fail> {
        if self.bit(base, src)? == 0 {
            return Ok(self.tree(base + 2 + pos_state * 8, 3, src)? as usize);
        }
        if self.bit(base + 1, src)? == 0 {
            return Ok(8 + self.tree(base + 2 + 128 + pos_state * 8, 3, src)? as usize);
        }
        Ok(16 + self.tree(base + 2 + 256, 8, src)? as usize)
    }

    fn symbol<S: ByteSrc>(&mut self, src: &mut S) -> Result<Symbol, Fail> {
        let pos_state = (self.dict.total as usize) & ((1 << self.pb) - 1);
        let state = self.state;
        if self.bit(IS_MATCH + state * 16 + pos_state, src)? == 0 {
            let lit_state = (((self.dict.total as usize) & ((1 << self.lp) - 1)) << self.lc) + (usize::from(self.dict.prev()) >> (8 - self.lc));
            let base = LITERAL + 0x300 * lit_state;
            let mut sym = 1usize;
            if state >= 7 {
                if self.reps[0] as usize >= self.dict.len {
                    return Err(Fail::Data);
                }
                let mut match_byte = usize::from(self.dict.get(self.reps[0] as usize + 1));
                let mut offs = 0x100usize;
                while sym < 0x100 {
                    match_byte <<= 1;
                    let m = match_byte & offs;
                    let b = self.bit(base + offs + m + sym, src)? as usize;
                    sym = (sym << 1) | b;
                    if b == 0 {
                        offs &= !m;
                    } else {
                        offs &= m;
                    }
                }
            } else {
                while sym < 0x100 {
                    sym = (sym << 1) | self.bit(base + sym, src)? as usize;
                }
            }
            self.state = match state {
                0..=3 => 0,
                4..=9 => state - 3,
                _ => state - 6,
            };
            return Ok(Symbol::Literal(sym as u8));
        }
        let len;
        if self.bit(IS_REP + state, src)? == 0 {
            self.reps[3] = self.reps[2];
            self.reps[2] = self.reps[1];
            self.reps[1] = self.reps[0];
            len = self.length(LEN_CODER, pos_state, src)?;
            self.state = if state < 7 { 7 } else { 10 };
            let len_state = len.min(3);
            let slot = self.tree(POS_SLOT + len_state * 64, 6, src)?;
            let dist = if slot < 4 {
                slot
            } else {
                let bits = (slot >> 1) - 1;
                let mut d = (2 | (slot & 1)) << bits;
                if slot < 14 {
                    d += self.reverse_tree(SPEC_POS + d as usize - slot as usize - 1, bits, src)?;
                } else {
                    d = d.wrapping_add(self.direct(bits - 4, src)? << 4);
                    d = d.wrapping_add(self.reverse_tree(ALIGN, 4, src)?);
                }
                d
            };
            if dist == u32::MAX {
                return Ok(Symbol::End);
            }
            self.reps[0] = dist;
        } else {
            if self.bit(IS_REP_G0 + state, src)? == 0 {
                if self.bit(IS_REP0_LONG + state * 16 + pos_state, src)? == 0 {
                    self.state = if state < 7 { 9 } else { 11 };
                    return Ok(Symbol::ShortRep);
                }
            } else {
                let dist;
                if self.bit(IS_REP_G1 + state, src)? == 0 {
                    dist = self.reps[1];
                } else {
                    if self.bit(IS_REP_G2 + state, src)? == 0 {
                        dist = self.reps[2];
                    } else {
                        dist = self.reps[3];
                        self.reps[3] = self.reps[2];
                    }
                    self.reps[2] = self.reps[1];
                }
                self.reps[1] = self.reps[0];
                self.reps[0] = dist;
            }
            len = self.length(REP_LEN_CODER, pos_state, src)?;
            self.state = if state < 7 { 8 } else { 11 };
        }
        Ok(Symbol::Match { dist: self.reps[0], len: len + MATCH_MIN })
    }

    fn emit(&mut self, b: u8, out: &mut Vec<u8>) {
        self.dict.put(b);
        out.push(b);
    }

    /// Applies a decoded symbol; `room` is how many more bytes may be produced.
    fn apply(&mut self, sym: Symbol, room: u64, out: &mut Vec<u8>) -> Result<bool, XzError> {
        match sym {
            Symbol::Literal(b) => {
                if room == 0 {
                    return Err(XzError::Data);
                }
                self.emit(b, out);
            }
            Symbol::ShortRep => {
                if room == 0 || self.reps[0] as usize >= self.dict.len {
                    return Err(XzError::Data);
                }
                let b = self.dict.get(self.reps[0] as usize + 1);
                self.emit(b, out);
            }
            Symbol::Match { dist, len } => {
                if dist as usize >= self.dict.len || len as u64 > room {
                    return Err(XzError::Data);
                }
                for _ in 0..len {
                    let b = self.dict.get(dist as usize + 1);
                    self.emit(b, out);
                }
            }
            Symbol::End => return Ok(true),
        }
        Ok(false)
    }
}

enum Lzma2Stage {
    Header,
    Packed { need: usize },
    Raw { left: usize },
    End,
}

/// An LZMA2 stream decoder.
pub(super) struct Lzma2 {
    lz: Lzma,
    stage: Lzma2Stage,
    head: Vec<u8>,
    kept: Vec<u8>,
    started: bool,
    packed_taken: usize,
    produced: usize,
    unpacked: usize,
    need_dict_reset: bool,
    need_props: bool,
    reset: u8,
}

impl Lzma2 {
    pub(super) fn new(dict_size: u32) -> Lzma2 {
        Lzma2 { lz: Lzma::new(dict_size), stage: Lzma2Stage::Header, head: Vec::new(), kept: Vec::new(), started: false, packed_taken: 0, produced: 0, unpacked: 0, need_dict_reset: true, need_props: true, reset: 0 }
    }

    fn header_len(control: u8) -> Result<usize, XzError> {
        match control {
            0 => Ok(1),
            1 | 2 => Ok(3),
            0x80..=0xBF => Ok(5),
            0xC0..=0xFF => Ok(6),
            _ => Err(XzError::Data),
        }
    }

    fn parse_header(&mut self) -> Result<(), XzError> {
        let h = &self.head;
        let control = h[0];
        if control == 0 {
            self.stage = Lzma2Stage::End;
            return Ok(());
        }
        if control == 1 || control == 2 {
            if control == 1 {
                self.lz.dict.reset();
                self.need_dict_reset = false;
                self.need_props = true;
            } else if self.need_dict_reset {
                return Err(XzError::Data);
            }
            let size = (usize::from(h[1]) << 8 | usize::from(h[2])) + 1;
            self.stage = Lzma2Stage::Raw { left: size };
            return Ok(());
        }
        if control >= 0xE0 {
            self.lz.dict.reset();
            self.need_dict_reset = false;
        } else if self.need_dict_reset {
            return Err(XzError::Data);
        }
        self.unpacked = ((usize::from(control & 0x1F) << 16) | (usize::from(h[1]) << 8) | usize::from(h[2])) + 1;
        let packed = (usize::from(h[3]) << 8 | usize::from(h[4])) + 1;
        if control >= 0xC0 {
            let props = u32::from(h[5]);
            if props >= 9 * 5 * 5 {
                return Err(XzError::Data);
            }
            let (lc, rest) = (props % 9, props / 9);
            let (lp, pb) = (rest % 5, rest / 5);
            if lc + lp > 4 {
                return Err(XzError::Data);
            }
            self.lz.set_props(lc, lp, pb);
            self.need_props = false;
        } else if self.need_props {
            return Err(XzError::Data);
        } else if control >= 0xA0 {
            self.lz.reset_state();
        }
        self.reset = control;
        self.kept.clear();
        self.started = false;
        self.packed_taken = 0;
        self.produced = 0;
        self.stage = Lzma2Stage::Packed { need: packed };
        Ok(())
    }

    /// Decodes as much of the current chunk as `data` (the chunk's next bytes) allows, symbol by
    /// symbol so that corrupt data is reported before the whole chunk has arrived. Returns the
    /// bytes taken and whether the chunk is complete.
    fn run_chunk(&mut self, data: &[u8], need: usize, out: &mut Vec<u8>) -> Result<(usize, bool), XzError> {
        let mut taken = 0usize;
        let start = out.len();
        if !self.started {
            let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
            match self.lz.init_rc(&mut src) {
                Ok(()) => {
                    self.kept.clear();
                    self.started = true;
                }
                Err(Fail::Need) => return self.chunk_wait(taken, need),
                Err(Fail::Data) => return Err(XzError::Data),
            }
        }
        while self.produced < self.unpacked {
            if out.len() - start >= LZMA1_BATCH {
                self.packed_taken += taken;
                return Ok((taken, false));
            }
            let saved = self.lz.save();
            self.lz.logging = self.kept.len() + (data.len() - taken) < 64;
            self.lz.undo.clear();
            let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
            let sym = self.lz.symbol(&mut src);
            let used = src.at;
            match sym {
                Ok(sym) => {
                    self.kept.drain(..used);
                    let before = out.len();
                    if self.lz.apply(sym, (self.unpacked - self.produced) as u64, out)? {
                        return Err(XzError::Data);
                    }
                    self.produced += out.len() - before;
                }
                Err(Fail::Need) => {
                    self.lz.restore(saved);
                    return self.chunk_wait(taken, need);
                }
                Err(Fail::Data) => return Err(XzError::Data),
            }
        }
        let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
        match self.lz.normalize(&mut src) {
            Ok(()) => {
                self.kept.clear();
                if self.lz.code != 0 || self.packed_taken + taken != need {
                    return Err(XzError::Data);
                }
                self.packed_taken = need;
                Ok((taken, true))
            }
            Err(Fail::Need) => self.chunk_wait(taken, need),
            Err(Fail::Data) => Err(XzError::Data),
        }
    }

    /// The chunk needs more input: fine while some of it is still to come, corrupt otherwise.
    fn chunk_wait(&mut self, taken: usize, need: usize) -> Result<(usize, bool), XzError> {
        self.packed_taken += taken;
        if self.packed_taken >= need {
            return Err(XzError::Data);
        }
        Ok((taken, false))
    }

    /// Takes input and appends decoded bytes to `out`. Returns how much input was used and
    /// whether the stream has ended. Stops after one chunk so the output stays bounded.
    pub(super) fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(usize, bool), XzError> {
        let mut used = 0usize;
        loop {
            match self.stage {
                Lzma2Stage::End => return Ok((used, true)),
                Lzma2Stage::Header => {
                    if self.head.is_empty() {
                        let Some(&c) = data.get(used) else { return Ok((used, false)) };
                        Lzma2::header_len(c)?;
                        self.head.push(c);
                        used += 1;
                    }
                    let need = Lzma2::header_len(self.head[0])?;
                    let take = (need - self.head.len()).min(data.len() - used);
                    self.head.extend_from_slice(&data[used..used + take]);
                    used += take;
                    if self.head.len() < need {
                        return Ok((used, false));
                    }
                    self.parse_header()?;
                    self.head.clear();
                }
                Lzma2Stage::Packed { need } => {
                    let end = (used + (need - self.packed_taken)).min(data.len());
                    let (taken, finished) = self.run_chunk(&data[used..end], need, out)?;
                    used += taken;
                    if finished {
                        self.stage = Lzma2Stage::Header;
                    }
                    return Ok((used, false));
                }
                Lzma2Stage::Raw { left } => {
                    let take = left.min(data.len() - used);
                    for &b in &data[used..used + take] {
                        self.lz.emit(b, out);
                    }
                    used += take;
                    if take < left {
                        self.stage = Lzma2Stage::Raw { left: left - take };
                        return Ok((used, false));
                    }
                    self.stage = Lzma2Stage::Header;
                    return Ok((used, false));
                }
            }
        }
    }
}

enum Lzma1Stage {
    Start,
    Run,
    Tail,
    End,
}

/// An LZMA1 stream decoder: with a known uncompressed size, or up to an end marker.
pub(super) struct Lzma1 {
    lz: Lzma,
    stage: Lzma1Stage,
    kept: Vec<u8>,
    remaining: Option<u64>,
}

const LZMA1_BATCH: usize = 1 << 15;

impl Lzma1 {
    pub(super) fn new(dict_size: u32, lc: u32, lp: u32, pb: u32, size: Option<u64>) -> Lzma1 {
        let mut lz = Lzma::new(dict_size);
        lz.set_props(lc, lp, pb);
        Lzma1 { lz, stage: Lzma1Stage::Start, kept: Vec::new(), remaining: size }
    }

    pub(super) fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(usize, bool), XzError> {
        let mut taken = 0usize;
        let start = out.len();
        loop {
            match self.stage {
                Lzma1Stage::End => return Ok((taken, true)),
                Lzma1Stage::Start => {
                    let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
                    match self.lz.init_rc(&mut src) {
                        Ok(()) => {
                            self.kept.clear();
                            self.stage = Lzma1Stage::Run;
                        }
                        Err(Fail::Need) => return Ok((taken, false)),
                        Err(Fail::Data) => return Err(XzError::Data),
                    }
                }
                Lzma1Stage::Run => {
                    if out.len() - start >= LZMA1_BATCH {
                        return Ok((taken, false));
                    }
                    if self.remaining == Some(0) {
                        self.stage = Lzma1Stage::Tail;
                        continue;
                    }
                    let saved = self.lz.save();
                    self.lz.logging = self.kept.len() + (data.len() - taken) < 64;
                    self.lz.undo.clear();
                    let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
                    let sym = self.lz.symbol(&mut src);
                    let used = src.at;
                    match sym {
                        Ok(sym) => {
                            self.kept.drain(..used);
                            let room = self.remaining.unwrap_or(u64::MAX);
                            let before = out.len();
                            let end = self.lz.apply(sym, room, out)?;
                            if let Some(r) = &mut self.remaining {
                                *r -= (out.len() - before) as u64;
                            }
                            if end {
                                self.stage = Lzma1Stage::Tail;
                                self.remaining = Some(0);
                            }
                        }
                        Err(Fail::Need) => {
                            self.lz.restore(saved);
                            return Ok((taken, false));
                        }
                        Err(Fail::Data) => return Err(XzError::Data),
                    }
                }
                Lzma1Stage::Tail => {
                    let mut src = Lazy { kept: &mut self.kept, at: 0, data, taken: &mut taken };
                    match self.lz.normalize(&mut src) {
                        Ok(()) => {
                            self.kept.clear();
                            if self.lz.code != 0 {
                                return Err(XzError::Data);
                            }
                            self.stage = Lzma1Stage::End;
                        }
                        Err(Fail::Need) => return Ok((taken, false)),
                        Err(Fail::Data) => return Err(XzError::Data),
                    }
                }
            }
        }
    }
}
