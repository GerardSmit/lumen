//! A small std-only compressor for ahead-of-time blob sections (kept function text, deferred
//! function bodies, bytecode): LZ77 over a 1 MiB window plus canonical Huffman coding of the
//! literal/length and distance symbols — DEFLATE's scheme in one block, with a wider distance
//! alphabet and its own framing. Compression runs at build time (in the `include_js!` proc
//! macro or a build script), so it spends effort on the match search; decompression is
//! table-driven and runs when a section is first needed.
//!
//! ## Framing
//! ```text
//! u8   method   0 = stored (the payload is the bytes), 1 = LZH
//! uv   raw_len  the decompressed length (LEB128)
//! LZH: 163 bytes of 4-bit code lengths (286 literal/length symbols, then 40 distance
//!      symbols; low nibble first), then the LSB-first bit stream, ended by symbol 256.
//! ```
//! Literal/length symbols are DEFLATE's (0-255 literals, 256 end, 257-285 lengths 3..258 with
//! extra bits). Distance symbol `c` covers `1..=4` directly (`c + 1`); above that it has
//! `c / 2 - 1` extra bits over the base `((2 | (c & 1)) << extra) + 1`, like DEFLATE's
//! 30 codes extended to 40 (distances up to 2^20).

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW: usize = 1 << 20;
const NLIT: usize = 286;
const NDIST: usize = 40;
const MAX_BITS: u8 = 15;
const EOB: usize = 256;

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

type R<T> = Result<T, String>;

fn bad() -> String {
    "lzh: corrupt data".to_string()
}

fn put_uv(out: &mut Vec<u8>, mut v: u64) {
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

fn get_uv(data: &[u8], pos: &mut usize) -> R<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos).ok_or_else(bad)?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift >= 64 {
            return Err(bad());
        }
    }
}

fn len_code(len: usize) -> usize {
    // The last code whose base is <= len.
    LEN_BASE.partition_point(|&b| b as usize <= len) - 1
}

fn dist_code(dist: usize) -> (usize, u32, u32) {
    let v = (dist - 1) as u32;
    if v < 4 {
        return (v as usize, 0, 0);
    }
    let hb = 31 - v.leading_zeros();
    let code = 2 * hb + ((v >> (hb - 1)) & 1);
    let extra = hb - 1;
    let base = (2 | (code & 1)) << extra;
    (code as usize, extra, v - base)
}

fn dist_base(code: usize) -> (u32, u32) {
    if code < 4 {
        return (code as u32 + 1, 0);
    }
    let extra = (code / 2 - 1) as u32;
    (((2 | (code as u32 & 1)) << extra) + 1, extra)
}

// ---- compression ------------------------------------------------------------------------------

enum Token {
    Lit(u8),
    Match { len: u16, dist: u32 },
}

/// LZ77 parse with hash chains and one-step lazy matching.
fn parse(input: &[u8]) -> Vec<Token> {
    const HASH_BITS: u32 = 16;
    const MAX_CHAIN: usize = 96;
    const GOOD_ENOUGH: usize = 64;
    let n = input.len();
    let mut tokens = Vec::with_capacity(n / 3 + 16);
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut prev = vec![u32::MAX; n];
    let hash = |i: usize| -> usize {
        let v = input[i] as u32 | (input[i + 1] as u32) << 8 | (input[i + 2] as u32) << 16;
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    let insert = |i: usize, head: &mut [u32], prev: &mut [u32]| {
        if i + MIN_MATCH <= n {
            let h = hash(i);
            prev[i] = head[h];
            head[h] = i as u32;
        }
    };
    let longest = |i: usize, head: &[u32], prev: &[u32]| -> (usize, usize) {
        if i + MIN_MATCH > n {
            return (0, 0);
        }
        let max = (n - i).min(MAX_MATCH);
        let (mut best, mut best_dist) = (0usize, 0usize);
        let mut cand = head[hash(i)];
        let mut chain = 0;
        while cand != u32::MAX && chain < MAX_CHAIN {
            let c = cand as usize;
            if i - c > WINDOW {
                break;
            }
            // Only a candidate that can beat the best so far is compared in full.
            if input[c + best.min(max - 1)] == input[i + best.min(max - 1)] {
                let mut l = 0;
                while l < max && input[c + l] == input[i + l] {
                    l += 1;
                }
                if l > best {
                    best = l;
                    best_dist = i - c;
                    if l >= max || l >= GOOD_ENOUGH * 4 {
                        break;
                    }
                }
            }
            cand = prev[c];
            chain += 1;
        }
        if best >= MIN_MATCH {
            (best, best_dist)
        } else {
            (0, 0)
        }
    };
    let mut i = 0;
    while i < n {
        let (len, dist) = longest(i, &head, &prev);
        if len == 0 {
            tokens.push(Token::Lit(input[i]));
            insert(i, &mut head, &mut prev);
            i += 1;
            continue;
        }
        // Lazy: a longer match one byte later wins over this one.
        insert(i, &mut head, &mut prev);
        if len < GOOD_ENOUGH && i + 1 < n {
            let (next_len, _) = longest(i + 1, &head, &prev);
            if next_len > len + 1 {
                tokens.push(Token::Lit(input[i]));
                i += 1;
                continue;
            }
        }
        tokens.push(Token::Match {
            len: len as u16,
            dist: dist as u32,
        });
        for j in i + 1..i + len {
            insert(j, &mut head, &mut prev);
        }
        i += len;
    }
    tokens
}

/// Huffman code lengths for `freq`, limited to `MAX_BITS` (frequencies are flattened until the
/// tree fits). A lone used symbol gets length 1.
fn code_lengths(freq: &[u32]) -> Vec<u8> {
    let mut f: Vec<u64> = freq.iter().map(|&x| x as u64).collect();
    loop {
        let used: Vec<usize> = (0..f.len()).filter(|&s| f[s] > 0).collect();
        let mut lens = vec![0u8; f.len()];
        match used.len() {
            0 => return lens,
            1 => {
                lens[used[0]] = 1;
                return lens;
            }
            _ => {}
        }
        // Nodes 0..used.len() are leaves; parents are appended.
        let mut parent = vec![usize::MAX; used.len() * 2];
        let mut heap = std::collections::BinaryHeap::new();
        for (k, &s) in used.iter().enumerate() {
            heap.push(std::cmp::Reverse((f[s], k)));
        }
        let mut next = used.len();
        while heap.len() > 1 {
            let std::cmp::Reverse((fa, a)) = heap.pop().unwrap();
            let std::cmp::Reverse((fb, b)) = heap.pop().unwrap();
            parent[a] = next;
            parent[b] = next;
            heap.push(std::cmp::Reverse((fa + fb, next)));
            next += 1;
        }
        let mut max = 0u32;
        for (k, &s) in used.iter().enumerate() {
            let mut d = 0u32;
            let mut x = k;
            while parent[x] != usize::MAX {
                x = parent[x];
                d += 1;
            }
            lens[s] = d as u8;
            max = max.max(d);
        }
        if max <= MAX_BITS as u32 {
            return lens;
        }
        for x in f.iter_mut().filter(|x| **x > 0) {
            *x = (*x >> 1).max(1);
        }
    }
}

/// Canonical codes for `lens`, bit-reversed for an LSB-first stream.
fn canonical_codes(lens: &[u8]) -> Vec<u32> {
    let mut count = [0u32; MAX_BITS as usize + 1];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; MAX_BITS as usize + 2];
    let mut code = 0u32;
    for bits in 1..=MAX_BITS as usize {
        code = (code + count[bits - 1]) << 1;
        next[bits] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                return 0;
            }
            let c = next[l as usize];
            next[l as usize] += 1;
            c.reverse_bits() >> (32 - l as u32)
        })
        .collect()
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, bits: u32, count: u32) {
        self.acc |= (bits as u64) << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// `input` framed as stored (uncompressed) data.
pub(crate) fn stored(input: &[u8]) -> Vec<u8> {
    let mut stored = Vec::with_capacity(input.len() + 6);
    stored.push(0);
    put_uv(&mut stored, input.len() as u64);
    stored.extend_from_slice(input);
    stored
}

/// Compress `input` (framed; see the module docs). Never larger than `input` plus the frame.
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    let stored = stored(input);
    if input.len() < 64 {
        return stored;
    }
    let tokens = parse(input);
    let mut lit_freq = [0u32; NLIT];
    let mut dist_freq = [0u32; NDIST];
    for t in &tokens {
        match *t {
            Token::Lit(b) => lit_freq[b as usize] += 1,
            Token::Match { len, dist } => {
                lit_freq[257 + len_code(len as usize)] += 1;
                dist_freq[dist_code(dist as usize).0] += 1;
            }
        }
    }
    lit_freq[EOB] += 1;
    let lit_lens = code_lengths(&lit_freq);
    let dist_lens = code_lengths(&dist_freq);
    let lit_codes = canonical_codes(&lit_lens);
    let dist_codes = canonical_codes(&dist_lens);
    let mut out = Vec::with_capacity(input.len() / 3 + 200);
    out.push(1);
    put_uv(&mut out, input.len() as u64);
    let all: Vec<u8> = lit_lens.iter().chain(dist_lens.iter()).copied().collect();
    for pair in all.chunks(2) {
        out.push(pair[0] | pair.get(1).copied().unwrap_or(0) << 4);
    }
    let mut w = BitWriter { out, acc: 0, n: 0 };
    for t in &tokens {
        match *t {
            Token::Lit(b) => w.put(lit_codes[b as usize], lit_lens[b as usize] as u32),
            Token::Match { len, dist } => {
                let lc = len_code(len as usize);
                w.put(lit_codes[257 + lc], lit_lens[257 + lc] as u32);
                if LEN_EXTRA[lc] > 0 {
                    w.put(len as u32 - LEN_BASE[lc] as u32, LEN_EXTRA[lc] as u32);
                }
                let (dc, extra, rest) = dist_code(dist as usize);
                w.put(dist_codes[dc], dist_lens[dc] as u32);
                if extra > 0 {
                    w.put(rest, extra);
                }
            }
        }
    }
    w.put(lit_codes[EOB], lit_lens[EOB] as u32);
    let out = w.finish();
    if out.len() < stored.len() {
        out
    } else {
        stored
    }
}

// ---- decompression ----------------------------------------------------------------------------

const FAST_BITS: u32 = 10;

/// A canonical Huffman decoder: a `FAST_BITS` lookup table (`symbol << 4 | length`, 0 = the
/// code is longer) and the per-length counts / sorted symbols for longer codes.
struct Decoder {
    fast: Vec<u16>,
    count: [u16; MAX_BITS as usize + 1],
    symbols: Vec<u16>,
}

impl Decoder {
    fn new(lens: &[u8]) -> R<Decoder> {
        let mut count = [0u16; MAX_BITS as usize + 1];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        // Reject over-subscribed code sets (a corrupt header).
        let mut left = 1i32;
        for &c in &count[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return Err(bad());
            }
        }
        let mut offs = [0u16; MAX_BITS as usize + 2];
        for l in 1..=MAX_BITS as usize {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbols = vec![0u16; offs[MAX_BITS as usize + 1] as usize];
        for (s, &l) in lens.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        let codes = canonical_codes(lens);
        let mut fast = vec![0u16; 1 << FAST_BITS];
        for (s, &l) in lens.iter().enumerate() {
            if l == 0 || l as u32 > FAST_BITS {
                continue;
            }
            let entry = (s as u16) << 4 | l as u16;
            let mut k = codes[s] as usize;
            while k < fast.len() {
                fast[k] = entry;
                k += 1 << l;
            }
        }
        Ok(Decoder {
            fast,
            count,
            symbols,
        })
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
}

impl BitReader<'_> {
    #[inline]
    fn refill(&mut self) {
        while self.n <= 56 {
            let b = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.acc |= (b as u64) << self.n;
            self.n += 8;
        }
    }
    #[inline]
    fn bits(&mut self, count: u32) -> u32 {
        if self.n < count {
            self.refill();
        }
        let v = (self.acc & ((1u64 << count) - 1)) as u32;
        self.acc >>= count;
        self.n -= count;
        v
    }
    #[inline]
    fn symbol(&mut self, d: &Decoder) -> R<usize> {
        if self.n < MAX_BITS as u32 {
            self.refill();
        }
        let e = d.fast[(self.acc & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            let l = (e & 15) as u32;
            self.acc >>= l;
            self.n -= l;
            return Ok((e >> 4) as usize);
        }
        // A code longer than the table: walk it canonically, one bit at a time.
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS as usize {
            code |= (self.acc & 1) as i32;
            self.acc >>= 1;
            self.n -= 1;
            let c = d.count[len] as i32;
            if code - first < c {
                return Ok(d.symbols[(index + code - first) as usize] as usize);
            }
            index += c;
            first = (first + c) << 1;
            code <<= 1;
        }
        Err(bad())
    }
}

/// Decompress a [`compress`] frame.
pub(crate) fn decompress(data: &[u8]) -> R<Vec<u8>> {
    let method = *data.first().ok_or_else(bad)?;
    let mut pos = 1;
    let len = usize::try_from(get_uv(data, &mut pos)?).map_err(|_| bad())?;
    match method {
        0 => {
            let body = data.get(pos..).ok_or_else(bad)?;
            if body.len() != len {
                return Err(bad());
            }
            Ok(body.to_vec())
        }
        1 => {
            let table = data.get(pos..pos + (NLIT + NDIST).div_ceil(2)).ok_or_else(bad)?;
            let mut lens = Vec::with_capacity(NLIT + NDIST + 1);
            for &b in table {
                lens.push(b & 15);
                lens.push(b >> 4);
            }
            let lit = Decoder::new(&lens[..NLIT])?;
            let dist = Decoder::new(&lens[NLIT..NLIT + NDIST])?;
            let mut r = BitReader {
                data,
                pos: pos + table.len(),
                acc: 0,
                n: 0,
            };
            // (`len` is untrusted until the stream proves it: bound the up-front reservation.)
            let mut out: Vec<u8> = Vec::with_capacity(len.min(data.len().saturating_mul(64)));
            loop {
                let s = r.symbol(&lit)?;
                if s < 256 {
                    if out.len() >= len {
                        return Err(bad());
                    }
                    out.push(s as u8);
                    continue;
                }
                if s == EOB {
                    break;
                }
                let lc = s - 257;
                if lc >= LEN_BASE.len() {
                    return Err(bad());
                }
                let m = LEN_BASE[lc] as usize + r.bits(LEN_EXTRA[lc] as u32) as usize;
                let dc = r.symbol(&dist)?;
                if dc >= NDIST {
                    return Err(bad());
                }
                let (base, extra) = dist_base(dc);
                let d = base as usize + r.bits(extra) as usize;
                if d > out.len() || out.len() + m > len {
                    return Err(bad());
                }
                let from = out.len() - d;
                if d >= m {
                    out.extend_from_within(from..from + m);
                } else {
                    for k in 0..m {
                        out.push(out[from + k]);
                    }
                }
            }
            // The stream must end within the data and produce exactly `len` bytes.
            if out.len() != len || r.pos > data.len() + 8 {
                return Err(bad());
            }
            Ok(out)
        }
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(input: &[u8]) -> usize {
        let c = compress(input);
        assert_eq!(decompress(&c).unwrap(), input, "len {}", input.len());
        c.len()
    }

    #[test]
    fn round_trips() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(&[7u8; 1000]);
        let js = "function f(a, b) { return a + b; }\nconst g = (x) => x * 2;\n".repeat(300);
        let n = round_trip(js.as_bytes());
        assert!(n * 10 < js.len(), "{n} of {}", js.len());
        // Pseudo-random bytes (incompressible: stored) and a skewed alphabet (long codes).
        let mut x = 0x1234_5678u32;
        let noise: Vec<u8> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        round_trip(&noise);
        let skewed: Vec<u8> = noise
            .iter()
            .map(|&b| b.trailing_zeros() as u8 * 17)
            .collect();
        round_trip(&skewed);
        // Far matches (beyond 32 KiB) and every distance code.
        let mut far = noise.clone();
        far.extend(std::iter::repeat_n(0u8, 70_000));
        far.extend_from_slice(&noise);
        round_trip(&far);
    }

    #[test]
    fn distance_codes_cover_the_window() {
        for d in [1usize, 2, 4, 5, 6, 7, 8, 9, 100, 32768, 32769, 1 << 19, WINDOW] {
            let (c, extra, rest) = dist_code(d);
            assert!(c < NDIST, "{d}");
            let (base, e2) = dist_base(c);
            assert_eq!(extra, e2);
            assert_eq!(base + rest, d as u32);
        }
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let js = "let answer = 42; function q() { return answer; }\n".repeat(50);
        let c = compress(js.as_bytes());
        for cut in [1, 2, 10, c.len() / 2, c.len() - 1] {
            let _ = decompress(&c[..cut]);
        }
        let mut flipped = c.clone();
        for k in (170..flipped.len()).step_by(7) {
            flipped[k] ^= 0x5a;
            let _ = decompress(&flipped);
        }
    }
}
