//! The language-neutral half of the pickle format: opcodes, the framed output buffer, the
//! integer and escape encodings, memo opcodes, and the unpickling stack. A host (CPython's
//! `_pickle` as implemented by `lumen-py`) supplies object traversal and I/O on top.

use crate::bigint::BigInt;

pub const HIGHEST_PROTOCOL: u8 = 5;
pub const DEFAULT_PROTOCOL: u8 = 4;
/// Elements written per `APPENDS`/`SETITEMS`/`ADDITEMS` batch.
pub const BATCHSIZE: usize = 1000;
/// Container nesting after which a pickler in fast mode starts watching for cycles.
pub const FAST_NESTING_LIMIT: i32 = 50;
/// A frame shorter than this is written without its `FRAME` header.
pub const FRAME_SIZE_MIN: usize = 4;
/// Frame length after which the output is flushed at an opcode boundary.
pub const FRAME_SIZE_TARGET: usize = 64 * 1024;
pub const FRAME_HEADER_SIZE: usize = 9;
/// Bytes requested from `peek()` when reading ahead.
pub const PREFETCH: usize = 8192 * 16;

pub mod op {
    pub const MARK: u8 = b'(';
    pub const STOP: u8 = b'.';
    pub const POP: u8 = b'0';
    pub const POP_MARK: u8 = b'1';
    pub const DUP: u8 = b'2';
    pub const FLOAT: u8 = b'F';
    pub const INT: u8 = b'I';
    pub const BININT: u8 = b'J';
    pub const BININT1: u8 = b'K';
    pub const LONG: u8 = b'L';
    pub const BININT2: u8 = b'M';
    pub const NONE: u8 = b'N';
    pub const PERSID: u8 = b'P';
    pub const BINPERSID: u8 = b'Q';
    pub const REDUCE: u8 = b'R';
    pub const STRING: u8 = b'S';
    pub const BINSTRING: u8 = b'T';
    pub const SHORT_BINSTRING: u8 = b'U';
    pub const UNICODE: u8 = b'V';
    pub const BINUNICODE: u8 = b'X';
    pub const APPEND: u8 = b'a';
    pub const BUILD: u8 = b'b';
    pub const GLOBAL: u8 = b'c';
    pub const DICT: u8 = b'd';
    pub const EMPTY_DICT: u8 = b'}';
    pub const APPENDS: u8 = b'e';
    pub const GET: u8 = b'g';
    pub const BINGET: u8 = b'h';
    pub const INST: u8 = b'i';
    pub const LONG_BINGET: u8 = b'j';
    pub const LIST: u8 = b'l';
    pub const EMPTY_LIST: u8 = b']';
    pub const OBJ: u8 = b'o';
    pub const PUT: u8 = b'p';
    pub const BINPUT: u8 = b'q';
    pub const LONG_BINPUT: u8 = b'r';
    pub const SETITEM: u8 = b's';
    pub const TUPLE: u8 = b't';
    pub const EMPTY_TUPLE: u8 = b')';
    pub const SETITEMS: u8 = b'u';
    pub const BINFLOAT: u8 = b'G';

    pub const PROTO: u8 = 0x80;
    pub const NEWOBJ: u8 = 0x81;
    pub const EXT1: u8 = 0x82;
    pub const EXT2: u8 = 0x83;
    pub const EXT4: u8 = 0x84;
    pub const TUPLE1: u8 = 0x85;
    pub const TUPLE2: u8 = 0x86;
    pub const TUPLE3: u8 = 0x87;
    pub const NEWTRUE: u8 = 0x88;
    pub const NEWFALSE: u8 = 0x89;
    pub const LONG1: u8 = 0x8a;
    pub const LONG4: u8 = 0x8b;

    pub const BINBYTES: u8 = b'B';
    pub const SHORT_BINBYTES: u8 = b'C';

    pub const SHORT_BINUNICODE: u8 = 0x8c;
    pub const BINUNICODE8: u8 = 0x8d;
    pub const BINBYTES8: u8 = 0x8e;
    pub const EMPTY_SET: u8 = 0x8f;
    pub const ADDITEMS: u8 = 0x90;
    pub const FROZENSET: u8 = 0x91;
    pub const NEWOBJ_EX: u8 = 0x92;
    pub const STACK_GLOBAL: u8 = 0x93;
    pub const MEMOIZE: u8 = 0x94;
    pub const FRAME: u8 = 0x95;

    pub const BYTEARRAY8: u8 = 0x96;
    pub const NEXT_BUFFER: u8 = 0x97;
    pub const READONLY_BUFFER: u8 = 0x98;
}

/// The two's complement little-endian bytes `LONG1`/`LONG4` carry: the shortest encoding, and
/// empty for zero.
pub fn encode_long(n: &BigInt) -> Vec<u8> {
    if n.is_zero() {
        return Vec::new();
    }
    let (neg, mag) = n.words();
    let nbytes = (n.bit_len() >> 3) + 1;
    let mut out: Vec<u8> = mag.iter().flat_map(|w| w.to_le_bytes()).chain(std::iter::repeat(0)).take(nbytes).collect();
    if neg {
        negate_le(&mut out);
        if nbytes > 1 && out[nbytes - 1] == 0xff && out[nbytes - 2] & 0x80 != 0 {
            out.pop();
        }
    }
    out
}

/// The integer a `LONG1`/`LONG4` payload holds.
pub fn decode_long(bytes: &[u8]) -> BigInt {
    let neg = bytes.last().is_some_and(|b| b & 0x80 != 0);
    let mut le = bytes.to_vec();
    if neg {
        negate_le(&mut le);
    }
    let mut mag: Vec<u64> = le
        .chunks(8)
        .map(|c| {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            u64::from_le_bytes(w)
        })
        .collect();
    while mag.last() == Some(&0) {
        mag.pop();
    }
    BigInt::from_words(neg && !mag.is_empty(), mag)
}

fn negate_le(bytes: &mut [u8]) {
    let mut carry = true;
    for b in bytes.iter_mut() {
        let (v, c) = (!*b).overflowing_add(carry as u8);
        *b = v;
        carry = c;
    }
}

/// The opcode and operand `save_long` writes for a value that fits in 32 bits.
pub fn encode_small_int(bin: bool, val: i64) -> Vec<u8> {
    if !bin {
        return format!("I{}\n", val).into_bytes();
    }
    let le = (val as i32).to_le_bytes();
    if le[3] != 0 || le[2] != 0 {
        let mut v = vec![op::BININT];
        v.extend_from_slice(&le);
        v
    } else if le[1] != 0 {
        vec![op::BININT2, le[0], le[1]]
    } else {
        vec![op::BININT1, le[0]]
    }
}

/// `GET`/`BINGET`/`LONG_BINGET` for memo slot `idx`.
pub fn memo_get_op(bin: bool, idx: usize) -> Result<Vec<u8>, &'static str> {
    if !bin {
        return Ok(format!("g{}\n", idx).into_bytes());
    }
    if idx < 256 {
        return Ok(vec![op::BINGET, idx as u8]);
    }
    if idx <= 0xffff_ffff {
        let mut v = vec![op::LONG_BINGET];
        v.extend_from_slice(&(idx as u32).to_le_bytes());
        return Ok(v);
    }
    Err("memo id too large for LONG_BINGET")
}

/// `MEMOIZE` (protocol 4+), or `PUT`/`BINPUT`/`LONG_BINPUT` for memo slot `idx`.
pub fn memo_put_op(proto: u8, bin: bool, idx: usize) -> Result<Vec<u8>, &'static str> {
    if proto >= 4 {
        return Ok(vec![op::MEMOIZE]);
    }
    if !bin {
        return Ok(format!("p{}\n", idx).into_bytes());
    }
    if idx < 256 {
        return Ok(vec![op::BINPUT, idx as u8]);
    }
    if idx <= 0xffff_ffff {
        let mut v = vec![op::LONG_BINPUT];
        v.extend_from_slice(&(idx as u32).to_le_bytes());
        return Ok(v);
    }
    Err("memo id too large for LONG_BINPUT")
}

/// `raw-unicode-escape` that also escapes backslash, NUL, newline, carriage return and
/// Ctrl-Z, as the protocol 0 `UNICODE` opcode needs.
pub fn raw_unicode_escape(chars: impl Iterator<Item = u32>) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::new();
    for ch in chars {
        if ch >= 0x10000 {
            out.extend_from_slice(b"\\U");
            for shift in (0..8).rev() {
                out.push(HEX[((ch >> (shift * 4)) & 0xf) as usize]);
            }
        } else if ch >= 256 || ch == '\\' as u32 || ch == 0 || ch == '\n' as u32 || ch == '\r' as u32 || ch == 0x1a {
            out.extend_from_slice(b"\\u");
            for shift in (0..4).rev() {
                out.push(HEX[((ch >> (shift * 4)) & 0xf) as usize]);
            }
        } else {
            out.push(ch as u8);
        }
    }
    out
}

/// An unsigned little-endian size of up to 8 bytes, `None` when it exceeds `isize::MAX`.
pub fn calc_binsize(bytes: &[u8]) -> Option<usize> {
    let mut x: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i >= 8 {
            if b != 0 {
                return None;
            }
            continue;
        }
        x |= (b as u64) << (8 * i);
    }
    usize::try_from(x).ok().filter(|&n| n <= isize::MAX as usize)
}

/// A little-endian integer of 1, 2 or 4 bytes: unsigned for 1 and 2 bytes, signed for 4.
pub fn calc_binint(bytes: &[u8]) -> i64 {
    let mut x: i64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        x |= (b as i64) << (8 * i);
    }
    if bytes.len() == 4 {
        x = x as i32 as i64;
    }
    x
}

/// C's `strtol(s, &end, 0)` for the `INT` opcode: the value and the index just past the number,
/// `None` when no digits were read or the value does not fit a 64-bit `long`.
pub fn strtol_base0(s: &[u8]) -> Option<(i64, usize)> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut base = 10u32;
    if i < s.len() && s[i] == b'0' {
        if i + 2 < s.len() && matches!(s[i + 1], b'x' | b'X') && (s[i + 2] as char).is_ascii_hexdigit() {
            base = 16;
            i += 2;
        } else {
            base = 8;
        }
    }
    let start = i;
    let mut acc: i128 = 0;
    while i < s.len() {
        let Some(d) = (s[i] as char).to_digit(base) else { break };
        acc = acc * base as i128 + d as i128;
        if acc > (i64::MAX as i128) + 1 {
            return None;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    let v = if neg { -acc } else { acc };
    i64::try_from(v).ok().map(|v| (v, i))
}

/// The output buffer of a pickler: bytes written so far, grouped into protocol 4 frames.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
    framing: bool,
    frame_start: Option<usize>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    pub fn framing(&self) -> bool {
        self.framing
    }

    pub fn set_framing(&mut self, on: bool) {
        self.framing = on;
    }

    /// Drops everything written and any open frame.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.frame_start = None;
    }

    pub fn write(&mut self, data: &[u8]) {
        if self.framing && self.frame_start.is_none() {
            self.frame_start = Some(self.buf.len());
            self.buf.extend_from_slice(&[0xFE; FRAME_HEADER_SIZE]);
        }
        self.buf.extend_from_slice(data);
    }

    pub fn write_byte(&mut self, b: u8) {
        self.write(&[b]);
    }

    /// Length of the open frame's payload; 0 when there is none.
    pub fn frame_len(&self) -> usize {
        match self.frame_start {
            Some(s) if self.framing => self.buf.len() - s - FRAME_HEADER_SIZE,
            _ => 0,
        }
    }

    /// Whether the open frame has reached the size at which it is flushed.
    pub fn frame_full(&self) -> bool {
        self.framing && self.frame_start.is_some() && self.frame_len() >= FRAME_SIZE_TARGET
    }

    /// Closes the open frame: a `FRAME` header for a payload of at least `FRAME_SIZE_MIN`
    /// bytes, otherwise the payload stays unframed.
    pub fn commit_frame(&mut self) {
        let Some(start) = self.frame_start else { return };
        if !self.framing {
            return;
        }
        let len = self.buf.len() - start - FRAME_HEADER_SIZE;
        if len >= FRAME_SIZE_MIN {
            self.buf[start] = op::FRAME;
            self.buf[start + 1..start + 9].copy_from_slice(&(len as u64).to_le_bytes());
        } else {
            self.buf.drain(start..start + FRAME_HEADER_SIZE);
        }
        self.frame_start = None;
    }

    /// The finished output (the open frame committed), leaving the buffer empty.
    pub fn take(&mut self) -> Vec<u8> {
        self.commit_frame();
        self.frame_start = None;
        std::mem::take(&mut self.buf)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Why an unpickling stack access failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackError {
    Underflow,
    UnexpectedMark,
    NoMark,
}

impl StackError {
    pub fn message(self) -> &'static str {
        match self {
            StackError::Underflow => "unpickling stack underflow",
            StackError::UnexpectedMark => "unexpected MARK found",
            StackError::NoMark => "could not find MARK",
        }
    }
}

/// The unpickling stack with its `MARK` positions. Items below the topmost mark (the fence)
/// cannot be popped or inspected.
pub struct Stack<T> {
    data: Vec<T>,
    marks: Vec<usize>,
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Stack { data: Vec::new(), marks: Vec::new() }
    }
}

impl<T> Stack<T> {
    pub fn new() -> Stack<T> {
        Stack::default()
    }

    pub fn reset(&mut self) {
        self.data.clear();
        self.marks.clear();
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Position of the topmost mark, 0 without one.
    pub fn fence(&self) -> usize {
        self.marks.last().copied().unwrap_or(0)
    }

    pub fn underflow(&self) -> StackError {
        if self.marks.is_empty() {
            StackError::Underflow
        } else {
            StackError::UnexpectedMark
        }
    }

    pub fn push(&mut self, v: T) {
        self.data.push(v);
    }

    pub fn pop(&mut self) -> Result<T, StackError> {
        if self.data.len() <= self.fence() {
            return Err(self.underflow());
        }
        Ok(self.data.pop().expect("length checked against the fence"))
    }

    pub fn top(&self) -> Result<&T, StackError> {
        if self.data.len() <= self.fence() {
            return Err(self.underflow());
        }
        Ok(&self.data[self.data.len() - 1])
    }

    pub fn top_mut(&mut self) -> Result<&mut T, StackError> {
        if self.data.len() <= self.fence() {
            return Err(self.underflow());
        }
        let n = self.data.len() - 1;
        Ok(&mut self.data[n])
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        self.data.get(i)
    }

    pub fn items(&self) -> &[T] {
        &self.data
    }

    /// Records a mark at the current top.
    pub fn mark(&mut self) {
        self.marks.push(self.data.len());
    }

    /// Removes the topmost mark and returns its position.
    pub fn marker(&mut self) -> Result<usize, StackError> {
        self.marks.pop().ok_or(StackError::NoMark)
    }

    /// `POP`: removes the top item, or the topmost mark when it sits at the top.
    pub fn pop_or_unmark(&mut self) -> Result<(), StackError> {
        if self.marks.last() == Some(&self.data.len()) {
            self.marks.pop();
            return Ok(());
        }
        self.pop().map(drop)
    }

    /// Removes and returns the items from `start` up, refusing to reach below the fence.
    pub fn take_from(&mut self, start: usize) -> Result<Vec<T>, StackError> {
        if start < self.fence() || start > self.data.len() {
            return Err(self.underflow());
        }
        Ok(self.data.split_off(start))
    }

    /// Removes and returns the items from `start` up.
    pub fn drain_from(&mut self, start: usize) -> Vec<T> {
        self.data.split_off(start.min(self.data.len()))
    }

    pub fn truncate(&mut self, n: usize) {
        self.data.truncate(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_round_trip() {
        for v in [0i64, 1, -1, 127, 128, -128, -129, 255, 256, 32767, -32768, 1 << 40, -(1 << 40)] {
            let n = BigInt::from_i64(v);
            assert_eq!(decode_long(&encode_long(&n)).to_i64(), Some(v));
        }
        assert_eq!(encode_long(&BigInt::from_i64(-128)), vec![0x80]);
        assert_eq!(encode_long(&BigInt::from_i64(128)), vec![0x80, 0x00]);
        assert_eq!(encode_long(&BigInt::from_i64(0)), Vec::<u8>::new());
    }

    #[test]
    fn small_ints() {
        assert_eq!(encode_small_int(true, 5), vec![op::BININT1, 5]);
        assert_eq!(encode_small_int(true, 256), vec![op::BININT2, 0, 1]);
        assert_eq!(encode_small_int(true, -1), vec![op::BININT, 255, 255, 255, 255]);
        assert_eq!(encode_small_int(false, -7), b"I-7\n".to_vec());
    }

    #[test]
    fn frames() {
        let mut w = Writer::new();
        w.set_framing(true);
        w.write(b"ab");
        w.commit_frame();
        assert_eq!(w.take(), b"ab".to_vec());
        w.write(b"abcdef");
        let out = w.take();
        assert_eq!(&out[..9], &[op::FRAME, 6, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&out[9..], b"abcdef");
    }

    #[test]
    fn strtol() {
        assert_eq!(strtol_base0(b"12\n"), Some((12, 2)));
        assert_eq!(strtol_base0(b"-0x1f"), Some((-31, 5)));
        assert_eq!(strtol_base0(b"010\n"), Some((8, 3)));
        assert_eq!(strtol_base0(b"99999999999999999999\n"), None);
    }
}
