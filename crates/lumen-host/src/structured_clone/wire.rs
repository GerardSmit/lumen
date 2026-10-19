//! The byte format of a structured clone: a tag byte then a type-specific payload. Integers are
//! little-endian. Objects register in the memory table before their contents are written, so a
//! repeat or a cycle is a back-reference (`T_REF` and an index).

use lumen::embed::{OpError, OpResult};

pub(super) const T_UNDEFINED: u8 = 0;
pub(super) const T_NULL: u8 = 1;
pub(super) const T_FALSE: u8 = 2;
pub(super) const T_TRUE: u8 = 3;
pub(super) const T_NUMBER: u8 = 4;
pub(super) const T_STRING: u8 = 5;
pub(super) const T_BIGINT: u8 = 6;
pub(super) const T_REF: u8 = 7;
/// `u32` length, `u32` entry count, then per entry a key (`T_KEY_INDEX` and a `u32`, or
/// `T_KEY_NAME` and a string) and a value.
pub(super) const T_ARRAY: u8 = 8;
/// `u32` entry count, then per entry a string key and a value.
pub(super) const T_OBJECT: u8 = 9;
pub(super) const T_DATE: u8 = 10;
pub(super) const T_REGEXP: u8 = 11;
pub(super) const T_MAP: u8 = 12;
pub(super) const T_SET: u8 = 13;
pub(super) const T_ARRAYBUFFER: u8 = 14;
pub(super) const T_TYPEDARRAY: u8 = 15;
pub(super) const T_DATAVIEW: u8 = 16;
/// A name byte, then optional message, stack and cause, each behind a presence byte.
pub(super) const T_ERROR: u8 = 17;
pub(super) const T_BOOLOBJ: u8 = 18;
pub(super) const T_NUMOBJ: u8 = 19;
pub(super) const T_STROBJ: u8 = 20;
pub(super) const T_SHARED: u8 = 21;
pub(super) const T_PORT: u8 = 22;
pub(super) const T_HOST: u8 = 23;
pub(super) const T_BLOB: u8 = 24;
/// Leading header: every transferred port, in transfer-list order, so a port that the value does
/// not reference still reaches the receiver (`MessageEvent.ports`).
pub(super) const T_PORTS: u8 = 25;
/// An object the local `structuredClone` swapped for a prepared copy (a transferred
/// `AbortSignal`): a `u32` index into the caller's list.
pub(super) const T_LOCAL: u8 = 26;
pub(super) const T_BIGINTOBJ: u8 = 27;
pub(super) const T_NATIVE_TRANSFER: u8 = 28;
pub(super) const T_NATIVE_VALUE: u8 = 29;
pub(super) const T_NATIVE_GRAPH_VALUE: u8 = 30;

pub(super) const KEY_INDEX: u8 = 0;
pub(super) const KEY_NAME: u8 = 1;

/// The error constructors a clone can name, by wire byte.
pub(super) const ERROR_NAMES: [&str; 7] = [
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "EvalError",
    "URIError",
];

pub(super) fn clone_error(message: impl Into<std::borrow::Cow<'static, str>>) -> OpError {
    OpError::new("DataCloneError", message)
}

pub(super) fn malformed() -> OpError {
    clone_error("malformed clone data")
}

/// The wire being written. A ceiling makes an oversized message fail while it is written: once a
/// write would pass it, nothing more is appended and `overflow` is set, which the writer checks
/// before every value.
pub(super) struct Sink {
    pub bytes: Vec<u8>,
    limit: usize,
    pub overflow: bool,
}

impl Default for Sink {
    fn default() -> Self {
        Self::with_limit(usize::MAX)
    }
}

impl Sink {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflow: false,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    #[inline]
    fn room(&mut self, extra: usize) -> bool {
        if self.bytes.len().saturating_add(extra) > self.limit {
            self.overflow = true;
        }
        !self.overflow
    }

    pub fn u8(&mut self, value: u8) {
        if self.room(1) {
            self.bytes.push(value);
        }
    }

    pub fn u32(&mut self, value: u32) {
        if self.room(4) {
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    pub fn f64(&mut self, value: f64) {
        if self.room(8) {
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    pub fn str(&mut self, value: &str) {
        if self.room(4 + value.len()) {
            self.bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
            self.bytes.extend_from_slice(value.as_bytes());
        }
    }

    pub fn raw(&mut self, value: &[u8]) {
        if self.room(value.len()) {
            self.bytes.extend_from_slice(value);
        }
    }

    pub fn patch_u32(&mut self, at: usize, value: u32) {
        if let Some(slot) = self.bytes.get_mut(at..at + 4) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }
}

pub(super) struct Source<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Source<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    pub fn take(&mut self, count: usize) -> OpResult<&'a [u8]> {
        let end = self.pos.checked_add(count).ok_or_else(malformed)?;
        let slice = self.bytes.get(self.pos..end).ok_or_else(malformed)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> OpResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> OpResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn f64(&mut self) -> OpResult<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    pub fn str(&mut self) -> OpResult<&'a str> {
        let length = self.u32()? as usize;
        std::str::from_utf8(self.take(length)?).map_err(|_| malformed())
    }
}
