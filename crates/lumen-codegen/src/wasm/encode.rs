//! Binary-format primitives: LEB128 integers, names, value types and section framing.

use crate::ir::Type;

pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        // Done once the rest is pure sign extension of the byte's bit 6.
        if (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn valtype(t: Type) -> u8 {
    match t {
        Type::I32 => 0x7f,
        Type::I64 => 0x7e,
        Type::F32 => 0x7d,
        Type::F64 => 0x7c,
    }
}

/// Append section `id` with a length-prefixed `body`.
pub fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// A vector: the item count, then the items `each` writes.
pub fn vec<T>(out: &mut Vec<u8>, items: &[T], mut each: impl FnMut(&mut Vec<u8>, &T)) {
    uleb(out, items.len() as u64);
    for it in items {
        each(out, it);
    }
}
