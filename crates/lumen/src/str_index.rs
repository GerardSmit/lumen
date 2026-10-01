//! UTF-16 positions in a UTF-8 engine string, without a UTF-16 copy.
//!
//! An all-ASCII string (see `LStr::ascii_hint`) needs nothing: byte index == unit index. For
//! any other string a [`UnitIndex`] records a checkpoint every [`STRIDE`] code units (a few
//! bytes per hundred units, against two bytes per unit for a UTF-16 copy) plus the last position
//! looked up, so a random access walks at most one stride and a sequential scan (`charCodeAt`
//! in a loop) advances from the previous position in O(1).
//!
//! Positions are `(byte, mid)`: the char starting at `byte` holds the unit, and `mid` says the
//! unit is the trailing (low) half of that char's surrogate pair.

use std::cell::Cell;

const STRIDE: usize = 64;

/// Units of the char whose lead byte is at `b[i]`: smuggled lone surrogates (U+10F800..,
/// `F4 8F A0..`) are one unit like any BMP char; other four-byte chars are a surrogate pair.
#[inline]
fn width(b: &[u8], i: usize) -> (usize, usize) {
    let c = b[i];
    if c < 0x80 {
        (1, 1)
    } else if c < 0xE0 {
        (2, 1)
    } else if c < 0xF0 {
        (3, 1)
    } else if c == 0xF4 && b[i + 1] == 0x8F && b[i + 2] >= 0xA0 {
        (4, 1)
    } else {
        (4, 2)
    }
}

/// From the char boundary `(byte, unit)`, the position of unit `t >= unit` (`t` at most the
/// string's unit length).
#[inline]
fn walk(b: &[u8], mut byte: usize, mut unit: usize, t: usize) -> (usize, bool) {
    loop {
        // ASCII run: one byte per unit.
        let run = b[byte..(byte + (t - unit)).min(b.len())]
            .iter()
            .position(|&c| c >= 0x80)
            .unwrap_or_else(|| (t - unit).min(b.len() - byte));
        byte += run;
        unit += run;
        if unit == t || byte >= b.len() {
            return (byte, false);
        }
        let (n, w) = width(b, byte);
        if unit + w > t {
            return (byte, true);
        }
        byte += n;
        unit += w;
    }
}

/// UTF-16 units of `b[from..to]` (both char boundaries).
#[inline]
fn count(b: &[u8], from: usize, to: usize) -> usize {
    let mut units = 0;
    let mut i = from;
    while i < to {
        if b[i] < 0x80 {
            i += 1;
            units += 1;
            continue;
        }
        let (n, w) = width(b, i);
        i += n;
        units += w;
    }
    units
}

/// The code unit at position `(byte, mid)` of `s`.
#[inline]
pub(crate) fn unit_at_pos(s: &str, byte: usize, mid: bool) -> u16 {
    let b0 = s.as_bytes()[byte];
    if b0 < 0x80 {
        return b0 as u16;
    }
    let c = s[byte..]
        .chars()
        .next()
        .expect("position inside the string");
    if let Some(u) = crate::jstr::smuggled(c) {
        return u;
    }
    let mut buf = [0u16; 2];
    c.encode_utf16(&mut buf)[mid as usize]
}

/// UTF-16 length of a string with no index (short strings).
pub(crate) fn unit_len(s: &str) -> usize {
    count(s.as_bytes(), 0, s.len())
}

/// Position of unit `t` in a string with no index (short strings).
pub(crate) fn locate_scan(s: &str, t: usize) -> (usize, bool) {
    walk(s.as_bytes(), 0, 0, t)
}

/// Unit offset of char boundary `byte` in a string with no index.
pub(crate) fn unit_of_byte_scan(s: &str, byte: usize) -> usize {
    count(s.as_bytes(), 0, byte)
}

/// See the module docs.
pub(crate) struct UnitIndex {
    /// UTF-16 length.
    pub(crate) len: usize,
    /// `marks[k]`: `(byte, unit)` of the first char boundary at or after unit `k * STRIDE`.
    marks: Box<[(u32, u32)]>,
    /// The char boundary of the last lookup.
    cursor: Cell<(u32, u32)>,
}

impl UnitIndex {
    pub(crate) fn new(s: &str) -> UnitIndex {
        let b = s.as_bytes();
        let mut marks = Vec::with_capacity(b.len() / STRIDE + 1);
        let (mut byte, mut unit, mut next) = (0usize, 0usize, 0usize);
        while byte < b.len() {
            if unit >= next {
                marks.push((byte as u32, unit as u32));
                next += STRIDE;
            }
            // Jump straight to the next mark through an ASCII run.
            let room = next - unit;
            let run = b[byte..(byte + room).min(b.len())]
                .iter()
                .position(|&c| c >= 0x80)
                .unwrap_or_else(|| room.min(b.len() - byte));
            byte += run;
            unit += run;
            if run == room || byte >= b.len() {
                continue;
            }
            let (n, w) = width(b, byte);
            byte += n;
            unit += w;
        }
        if unit >= next || marks.is_empty() {
            marks.push((byte as u32, unit as u32));
        }
        UnitIndex {
            len: unit,
            marks: marks.into_boxed_slice(),
            cursor: Cell::new((0, 0)),
        }
    }

    /// Position of unit `t <= len` in `s` (the string this index was built for).
    pub(crate) fn locate(&self, s: &str, t: usize) -> (usize, bool) {
        let mut k = (t / STRIDE).min(self.marks.len() - 1);
        if self.marks[k].1 as usize > t {
            k -= 1;
        }
        let (mut byte, mut unit) = (self.marks[k].0 as usize, self.marks[k].1 as usize);
        let (cb, cu) = self.cursor.get();
        if cu as usize <= t && cu as usize > unit {
            (byte, unit) = (cb as usize, cu as usize);
        }
        let (pb, mid) = walk(s.as_bytes(), byte, unit, t);
        self.cursor.set((pb as u32, (t - mid as usize) as u32));
        (pb, mid)
    }

    /// Code unit `t` of `s`; `None` past the end.
    #[inline]
    pub(crate) fn unit_at(&self, s: &str, t: usize) -> Option<u16> {
        if t >= self.len {
            return None;
        }
        let b = s.as_bytes();
        let (cb, cu) = self.cursor.get();
        let (cb, cu) = (cb as usize, cu as usize);
        // Sequential scan: the unit at or just after the last position.
        if t == cu || t == cu + 1 {
            let (n, w) = width(b, cb);
            if t == cu || w == 2 {
                return Some(unit_at_pos(s, cb, t != cu));
            }
            let nb = cb + n;
            self.cursor.set((nb as u32, t as u32));
            return Some(unit_at_pos(s, nb, false));
        }
        let (pb, mid) = self.locate(s, t);
        Some(unit_at_pos(s, pb, mid))
    }

    /// Unit offset of the char boundary `byte` of `s`.
    pub(crate) fn unit_of_byte(&self, s: &str, byte: usize) -> usize {
        let k = self.marks.partition_point(|m| m.0 as usize <= byte) - 1;
        let (mut from, mut unit) = (self.marks[k].0 as usize, self.marks[k].1 as usize);
        let (cb, cu) = self.cursor.get();
        if cb as usize <= byte && cb as usize > from {
            (from, unit) = (cb as usize, cu as usize);
        }
        unit + count(s.as_bytes(), from, byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(s: &str) {
        let units = crate::jstr::units(s);
        let ix = UnitIndex::new(s);
        assert_eq!(ix.len, units.len());
        assert_eq!(unit_len(s), units.len());
        // Forward, backward and strided lookups all agree with the UTF-16 copy.
        let order: Vec<usize> = (0..units.len())
            .chain((0..units.len()).rev())
            .chain((0..units.len()).step_by(37))
            .collect();
        for t in order {
            let (b, mid) = ix.locate(s, t);
            assert_eq!(unit_at_pos(s, b, mid), units[t], "unit {t}");
            assert_eq!((b, mid), locate_scan(s, t));
            assert_eq!(ix.unit_at(s, t), Some(units[t]));
            assert_eq!(ix.unit_at(s, t + 1), units.get(t + 1).copied());
            if !mid {
                assert_eq!(ix.unit_of_byte(s, b), t);
                assert_eq!(unit_of_byte_scan(s, b), t);
            }
        }
        assert_eq!(ix.locate(s, units.len()), (s.len(), false));
        assert_eq!(ix.unit_of_byte(s, s.len()), units.len());
    }

    #[test]
    fn index_matches_utf16_copy() {
        check("");
        check("abc");
        check(&"a".repeat(300));
        let mixed: String = (0..2000)
            .map(|i| match i % 97 {
                3 => "é".to_string(),
                10 => "中".to_string(),
                20 => "😀".to_string(),
                30 => crate::jstr::from_units(&[0xD800]),
                40 => crate::jstr::from_units(&[0xDFFF]),
                _ => ((b'a' + (i % 26) as u8) as char).to_string(),
            })
            .collect();
        check(&mixed);
        check(&"😀".repeat(200));
        check(&format!("{}😀{}", "x".repeat(63), "y".repeat(100)));
        check(&format!("{}😀", "x".repeat(127)));
    }

    /// String methods on non-ASCII strings (short ones walked, long ones indexed) against naive
    /// implementations over the code-unit array the string was built from.
    #[test]
    fn methods_match_code_unit_reference() {
        let source = r#"
        function assert(c, m) { if (!c) throw new Error(m); }
        function eqU(s, u, m) {
            assert(s.length === u.length, m + ' length ' + s.length + ' ' + u.length);
            for (let k = 0; k < u.length; k++) assert(s.charCodeAt(k) === u[k], m + ' unit ' + k);
        }
        function find(u, n, from) {
            for (let k = Math.max(0, from); k + n.length <= u.length; k++) {
                let ok = true;
                for (let j = 0; j < n.length; j++) if (u[k + j] !== n[j]) { ok = false; break; }
                if (ok) return k;
            }
            return -1;
        }
        function findLast(u, n, lim) {
            let r = -1;
            for (let k = 0; k <= lim && k + n.length <= u.length; k++) {
                let ok = true;
                for (let j = 0; j < n.length; j++) if (u[k + j] !== n[j]) { ok = false; break; }
                if (ok) r = k;
            }
            return r;
        }
        const pieces = [[0x61], [0x62], [0xE9], [0x4E2D], [0xD83D, 0xDE00], [0xD800], [0xDC00],
            [0xDBFF, 0xDFFF], [0x7A], [0x20]];
        let seed = 7;
        const rnd = (n) => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) % n;
        for (const size of [5, 20, 300]) {
            const u = [];
            for (let k = 0; k < size; k++) u.push(...pieces[rnd(pieces.length)]);
            const s = String.fromCharCode(...u);
            eqU(s, u, 'whole');
            for (let k = 0; k < u.length; k++) {
                assert(s[k] === String.fromCharCode(u[k]), 'index ' + k);
                assert(s.at(-1 - k) === String.fromCharCode(u[u.length - 1 - k]), 'at ' + k);
                const cp = s.codePointAt(k);
                const pair = u[k] >= 0xD800 && u[k] < 0xDC00 && u[k + 1] >= 0xDC00 && u[k + 1] < 0xE000;
                assert(cp === (pair ? 0x10000 + ((u[k] - 0xD800) << 10) + (u[k + 1] - 0xDC00) : u[k]), 'cp ' + k);
            }
            assert(s[u.length] === undefined && s.charCodeAt(u.length) !== s.charCodeAt(u.length), 'oob');
            for (let t = 0; t < 60; t++) {
                const a = rnd(u.length + 2), b = rnd(u.length + 2);
                eqU(s.slice(a, b), u.slice(a, b), 'slice ' + a + ' ' + b);
                eqU(s.slice(-a, -b || undefined), u.slice(-a, -b || undefined), 'slice- ' + a + ' ' + b);
                eqU(s.substring(a, b), u.slice(Math.min(a, b), Math.max(a, b)), 'substring ' + a + ' ' + b);
                eqU(s.substr(a, b), u.slice(a, a + b), 'substr ' + a + ' ' + b);
                const nl = rnd(4), ns = rnd(u.length);
                const n = u.slice(ns, ns + nl), ndl = String.fromCharCode(...n);
                assert(s.indexOf(ndl, a) === find(u, n, Math.min(a, u.length)), 'indexOf ' + a + ' ' + ns + ' ' + nl);
                assert(s.includes(ndl, a) === (find(u, n, Math.min(a, u.length)) >= 0), 'includes');
                assert(s.lastIndexOf(ndl, a) === findLast(u, n, Math.min(a, u.length)), 'lastIndexOf ' + a + ' ' + ns + ' ' + nl);
                assert(s.lastIndexOf(ndl) === findLast(u, n, u.length), 'lastIndexOf all');
                const e = Math.min(a, u.length);
                assert(s.startsWith(ndl, a) === (find(u.slice(0, e + n.length), n, e) === e), 'startsWith ' + a);
                assert(s.endsWith(ndl, a) === (e >= n.length && find(u, n, e - n.length) === e - n.length), 'endsWith ' + a);
            }
        }
        'passed'
        "#;
        for tier in [
            crate::bytecode::Tier::Interp,
            crate::bytecode::Tier::Bytecode,
        ] {
            let mut engine = crate::Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            match engine.eval(source, false).unwrap() {
                crate::Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                crate::Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }
}
