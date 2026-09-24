//! Message digests for `node:crypto` (FIPS 180-4 SHA-1 / SHA-2, RFC 1321 MD5), HMAC (RFC 2104),
//! PBKDF2 (RFC 8018) and HKDF (RFC 5869) — std-only, streaming, and bit-exact with Node (checked
//! against the FIPS/RFC vectors below and against `node -e`). SHA-1 and SHA-256 use the x86 SHA
//! extensions when the CPU has them (runtime-detected; the scalar code is the fallback and the
//! reference the tests compare them against).

// ---- algorithm table ---------------------------------------------------------------------------

/// A digest algorithm Node's `createHash` knows by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
}

impl Algo {
    /// Node/OpenSSL names, case-insensitive (`sha256`, `SHA-256`, `RSA-SHA256`, ...).
    pub fn from_name(name: &str) -> Option<Algo> {
        let lower = name.to_ascii_lowercase();
        let n = lower.strip_prefix("rsa-").unwrap_or(&lower);
        Some(match n {
            "md5" | "ssl3-md5" | "md5withrsaencryption" => Algo::Md5,
            "sha1" | "sha-1" | "ssl3-sha1" | "sha1withrsaencryption" => Algo::Sha1,
            "sha224" | "sha-224" | "sha224withrsaencryption" => Algo::Sha224,
            "sha256" | "sha-256" | "sha256withrsaencryption" => Algo::Sha256,
            "sha384" | "sha-384" | "sha384withrsaencryption" => Algo::Sha384,
            "sha512" | "sha-512" | "sha512withrsaencryption" => Algo::Sha512,
            "sha512-224" | "sha-512/224" | "sha512-224withrsaencryption" => Algo::Sha512_224,
            "sha512-256" | "sha-512/256" | "sha512-256withrsaencryption" => Algo::Sha512_256,
            _ => return None,
        })
    }

    /// Digest length in bytes.
    pub fn out_len(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 => 20,
            Algo::Sha224 | Algo::Sha512_224 => 28,
            Algo::Sha256 | Algo::Sha512_256 => 32,
            Algo::Sha384 => 48,
            Algo::Sha512 => 64,
        }
    }

    /// Compression block length in bytes (the HMAC block size).
    pub fn block_len(self) -> usize {
        match self {
            Algo::Md5 | Algo::Sha1 | Algo::Sha224 | Algo::Sha256 => 64,
            _ => 128,
        }
    }
}

// ---- streaming hasher --------------------------------------------------------------------------

#[derive(Clone)]
enum State {
    Md5([u32; 4]),
    Sha1([u32; 5]),
    Sha256([u32; 8]),
    Sha512([u64; 8]),
}

/// An in-progress digest. `Clone` is cheap (a few hundred bytes), which is what `Hash.copy()`,
/// HMAC's precomputed key states and PBKDF2's inner loop rely on.
#[derive(Clone)]
pub struct Hasher {
    algo: Algo,
    state: State,
    buf: [u8; 128],
    buf_len: usize,
    /// Total bytes absorbed.
    len: u64,
}

impl Hasher {
    pub fn new(algo: Algo) -> Hasher {
        let state = match algo {
            Algo::Md5 => State::Md5([0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476]),
            Algo::Sha1 => State::Sha1([0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0]),
            Algo::Sha224 => State::Sha256([
                0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7,
                0xbefa4fa4,
            ]),
            Algo::Sha256 => State::Sha256([
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ]),
            Algo::Sha384 => State::Sha512([
                0xcbbb9d5dc1059ed8,
                0x629a292a367cd507,
                0x9159015a3070dd17,
                0x152fecd8f70e5939,
                0x67332667ffc00b31,
                0x8eb44a8768581511,
                0xdb0c2e0d64f98fa7,
                0x47b5481dbefa4fa4,
            ]),
            Algo::Sha512 => State::Sha512([
                0x6a09e667f3bcc908,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ]),
            Algo::Sha512_224 => State::Sha512([
                0x8c3d37c819544da2,
                0x73e1996689dcd4d6,
                0x1dfab7ae32ff9c82,
                0x679dd514582f9fcf,
                0x0f6d2b697bd44da8,
                0x77e36f7304c48942,
                0x3f9d85a86a1d36c8,
                0x1112e6ad91d692a1,
            ]),
            Algo::Sha512_256 => State::Sha512([
                0x22312194fc2bf72c,
                0x9f555fa3c84c64c2,
                0x2393b86b6f53b151,
                0x963877195940eabd,
                0x96283ee2a88effe3,
                0xbe5e1e2553863992,
                0x2b0199fc2c85b8aa,
                0x0eb72ddc81c52ca2,
            ]),
        };
        Hasher {
            algo,
            state,
            buf: [0; 128],
            buf_len: 0,
            len: 0,
        }
    }

    pub fn algo(&self) -> Algo {
        self.algo
    }

    /// Compress whole blocks (`blocks.len()` is a multiple of the block length).
    fn compress(&mut self, blocks: &[u8]) {
        match &mut self.state {
            State::Md5(s) => md5_compress(s, blocks),
            State::Sha1(s) => sha1_compress(s, blocks),
            State::Sha256(s) => sha256_compress(s, blocks),
            State::Sha512(s) => sha512_compress(s, blocks),
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        let bl = self.algo.block_len();
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (bl - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < bl {
                return;
            }
            let block = self.buf;
            self.compress(&block[..bl]);
            self.buf_len = 0;
        }
        let whole = data.len() - data.len() % bl;
        if whole > 0 {
            self.compress(&data[..whole]);
        }
        let rest = &data[whole..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Finish into `out` (at least `out_len` bytes; exactly `out_len` are written).
    pub fn finish_into(mut self, out: &mut [u8]) {
        let bl = self.algo.block_len();
        let len_bytes = if bl == 128 { 16 } else { 8 };
        let bits = (self.len as u128) * 8;
        let mut tail = [0u8; 256];
        let n = self.buf_len;
        tail[..n].copy_from_slice(&self.buf[..n]);
        tail[n] = 0x80;
        let total = if n + 1 + len_bytes <= bl { bl } else { 2 * bl };
        match self.state {
            State::Md5(_) => tail[total - 8..total].copy_from_slice(&(bits as u64).to_le_bytes()),
            State::Sha512(_) => tail[total - 16..total].copy_from_slice(&bits.to_be_bytes()),
            _ => tail[total - 8..total].copy_from_slice(&(bits as u64).to_be_bytes()),
        }
        self.compress(&tail[..total]);
        let ol = self.algo.out_len();
        match &self.state {
            State::Md5(s) => {
                for (c, w) in out[..16].chunks_exact_mut(4).zip(s) {
                    c.copy_from_slice(&w.to_le_bytes());
                }
            }
            State::Sha1(s) => {
                for (c, w) in out[..20].chunks_exact_mut(4).zip(s) {
                    c.copy_from_slice(&w.to_be_bytes());
                }
            }
            State::Sha256(s) => {
                let mut full = [0u8; 32];
                for (c, w) in full.chunks_exact_mut(4).zip(s) {
                    c.copy_from_slice(&w.to_be_bytes());
                }
                out[..ol].copy_from_slice(&full[..ol]);
            }
            State::Sha512(s) => {
                let mut full = [0u8; 64];
                for (c, w) in full.chunks_exact_mut(8).zip(s) {
                    c.copy_from_slice(&w.to_be_bytes());
                }
                out[..ol].copy_from_slice(&full[..ol]);
            }
        }
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = vec![0u8; self.algo.out_len()];
        self.finish_into(&mut out);
        out
    }
}

/// One-shot digest.
pub fn digest(algo: Algo, data: &[u8]) -> Vec<u8> {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

// ---- HMAC ---------------------------------------------------------------------------------------

/// HMAC with the key already absorbed: `inner` has consumed `key ^ ipad`, `outer` `key ^ opad`.
#[derive(Clone)]
pub struct Hmac {
    inner: Hasher,
    outer: Hasher,
}

impl Hmac {
    pub fn new(algo: Algo, key: &[u8]) -> Hmac {
        let bl = algo.block_len();
        let mut k = [0u8; 128];
        if key.len() > bl {
            let d = digest(algo, key);
            k[..d.len()].copy_from_slice(&d);
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0u8; 128];
        let mut opad = [0u8; 128];
        for i in 0..bl {
            ipad[i] = k[i] ^ 0x36;
            opad[i] = k[i] ^ 0x5c;
        }
        let mut inner = Hasher::new(algo);
        inner.update(&ipad[..bl]);
        let mut outer = Hasher::new(algo);
        outer.update(&opad[..bl]);
        Hmac { inner, outer }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finish_into(self, out: &mut [u8]) {
        let mut ih = [0u8; 64];
        let ol = self.inner.algo().out_len();
        self.inner.finish_into(&mut ih);
        let mut outer = self.outer;
        outer.update(&ih[..ol]);
        outer.finish_into(out);
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = vec![0u8; self.inner.algo().out_len()];
        self.finish_into(&mut out);
        out
    }
}

pub fn hmac(algo: Algo, key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut h = Hmac::new(algo, key);
    h.update(data);
    h.finish()
}

// ---- KDFs ---------------------------------------------------------------------------------------

/// PBKDF2-HMAC (RFC 8018 §5.2).
pub fn pbkdf2(algo: Algo, password: &[u8], salt: &[u8], iterations: u32, keylen: usize) -> Vec<u8> {
    let hlen = algo.out_len();
    let keyed = Hmac::new(algo, password);
    let mut out = vec![0u8; keylen];
    let mut u = [0u8; 64];
    let mut t = [0u8; 64];
    for (i, chunk) in out.chunks_mut(hlen).enumerate() {
        let mut h = keyed.clone();
        h.update(salt);
        h.update(&(i as u32 + 1).to_be_bytes());
        h.finish_into(&mut u);
        t[..hlen].copy_from_slice(&u[..hlen]);
        for _ in 1..iterations {
            let mut h = keyed.clone();
            h.update(&u[..hlen]);
            h.finish_into(&mut u);
            for (a, b) in t[..hlen].iter_mut().zip(&u[..hlen]) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    out
}

/// HKDF extract-then-expand (RFC 5869). `keylen` must be at most `255 * out_len`.
pub fn hkdf(algo: Algo, ikm: &[u8], salt: &[u8], info: &[u8], keylen: usize) -> Vec<u8> {
    let hlen = algo.out_len();
    let zero = [0u8; 64];
    let salt = if salt.is_empty() { &zero[..hlen] } else { salt };
    let prk = hmac(algo, salt, ikm);
    let keyed = Hmac::new(algo, &prk);
    let mut out = Vec::with_capacity(keylen);
    let mut prev: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < keylen {
        let mut h = keyed.clone();
        h.update(&prev);
        h.update(info);
        h.update(&[i]);
        prev = h.finish();
        let take = (keylen - out.len()).min(hlen);
        out.extend_from_slice(&prev[..take]);
        i = i.wrapping_add(1);
    }
    out
}

// ---- MD5 (RFC 1321) -----------------------------------------------------------------------------

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

#[rustfmt::skip]
const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

fn md5_compress(s: &mut [u32; 4], blocks: &[u8]) {
    for b in blocks.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (w, c) in m.iter_mut().zip(b.chunks_exact(4)) {
            *w = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let [mut a, mut bb, mut c, mut d] = *s;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((bb & c) | (!bb & d), i),
                1 => ((d & bb) | (!d & c), (5 * i + 1) % 16),
                2 => (bb ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (bb | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = bb;
            bb = bb.wrapping_add(f.rotate_left(MD5_S[i]));
        }
        s[0] = s[0].wrapping_add(a);
        s[1] = s[1].wrapping_add(bb);
        s[2] = s[2].wrapping_add(c);
        s[3] = s[3].wrapping_add(d);
    }
}

// ---- SHA-1 --------------------------------------------------------------------------------------

fn sha1_compress(s: &mut [u32; 5], blocks: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    if shani() {
        // SAFETY: the CPU supports the SHA extensions (checked at runtime).
        unsafe { x86::sha1_blocks(s, blocks) };
        return;
    }
    sha1_compress_soft(s, blocks)
}

fn sha1_compress_soft(s: &mut [u32; 5], blocks: &[u8]) {
    for b in blocks.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (x, c) in w.iter_mut().zip(b.chunks_exact(4)) {
            *x = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut bb, mut c, mut d, mut e] = *s;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((bb & c) | (!bb & d), 0x5a827999),
                1 => (bb ^ c ^ d, 0x6ed9eba1),
                2 => ((bb & c) | (bb & d) | (c & d), 0x8f1bbcdc),
                _ => (bb ^ c ^ d, 0xca62c1d6u32),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = bb.rotate_left(30);
            bb = a;
            a = t;
        }
        s[0] = s[0].wrapping_add(a);
        s[1] = s[1].wrapping_add(bb);
        s[2] = s[2].wrapping_add(c);
        s[3] = s[3].wrapping_add(d);
        s[4] = s[4].wrapping_add(e);
    }
}

// ---- SHA-256 ------------------------------------------------------------------------------------

#[rustfmt::skip]
static K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_compress(s: &mut [u32; 8], blocks: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    if shani() {
        // SAFETY: the CPU supports the SHA extensions (checked at runtime).
        unsafe { x86::sha256_blocks(s, blocks) };
        return;
    }
    sha256_compress_soft(s, blocks)
}

fn sha256_compress_soft(s: &mut [u32; 8], blocks: &[u8]) {
    for b in blocks.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (x, c) in w.iter_mut().zip(b.chunks_exact(4)) {
            *x = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut h] = *s;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K256[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (v, add) in s.iter_mut().zip([a, bb, c, d, e, f, g, h]) {
            *v = v.wrapping_add(add);
        }
    }
}

// ---- SHA-512 ------------------------------------------------------------------------------------

#[rustfmt::skip]
const K512: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

fn sha512_compress(s: &mut [u64; 8], blocks: &[u8]) {
    for b in blocks.chunks_exact(128) {
        let mut w = [0u64; 80];
        for (x, c) in w.iter_mut().zip(b.chunks_exact(8)) {
            *x = u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut h] = *s;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K512[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (v, add) in s.iter_mut().zip([a, bb, c, d, e, f, g, h]) {
            *v = v.wrapping_add(add);
        }
    }
}

// ---- x86 SHA extensions -------------------------------------------------------------------------

/// Whether the CPU has the SHA extensions (plus the SSE levels the kernels use). `LUMEN_NO_SHANI`
/// forces the portable code (for comparing the two).
#[cfg(target_arch = "x86_64")]
fn shani() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHE: AtomicU8 = AtomicU8::new(0);
    match CACHE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let yes = std::env::var_os("LUMEN_NO_SHANI").is_none()
                && std::arch::is_x86_feature_detected!("sha")
                && std::arch::is_x86_feature_detected!("sse4.1")
                && std::arch::is_x86_feature_detected!("ssse3");
            CACHE.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
            yes
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// SHA-256 over whole 64-byte blocks with SHA-NI (the Intel reference schedule).
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub unsafe fn sha256_blocks(state: &mut [u32; 8], blocks: &[u8]) {
        let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
        let sp = state.as_ptr() as *const __m128i;
        let dcba = _mm_loadu_si128(sp);
        let efgh = _mm_loadu_si128(sp.add(1));
        let cdab = _mm_shuffle_epi32(dcba, 0xb1);
        let efgh = _mm_shuffle_epi32(efgh, 0x1b);
        let mut abef = _mm_alignr_epi8(cdab, efgh, 8);
        let mut cdgh = _mm_blend_epi16(efgh, cdab, 0xf0);
        let k = super::K256.as_ptr() as *const __m128i;

        macro_rules! rounds4 {
            ($w:expr, $i:expr) => {{
                let t1 = _mm_add_epi32($w, _mm_loadu_si128(k.add($i)));
                cdgh = _mm_sha256rnds2_epu32(cdgh, abef, t1);
                let t2 = _mm_shuffle_epi32(t1, 0x0e);
                abef = _mm_sha256rnds2_epu32(abef, cdgh, t2);
            }};
        }
        macro_rules! schedule {
            ($v0:expr, $v1:expr, $v2:expr, $v3:expr) => {{
                let t1 = _mm_sha256msg1_epu32($v0, $v1);
                let t2 = _mm_alignr_epi8($v3, $v2, 4);
                let t3 = _mm_add_epi32(t1, t2);
                _mm_sha256msg2_epu32(t3, $v3)
            }};
        }

        for b in blocks.chunks_exact(64) {
            let abef_save = abef;
            let cdgh_save = cdgh;
            let dp = b.as_ptr() as *const __m128i;
            let mut w0 = _mm_shuffle_epi8(_mm_loadu_si128(dp), mask);
            let mut w1 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(1)), mask);
            let mut w2 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(2)), mask);
            let mut w3 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(3)), mask);
            let mut w4;
            rounds4!(w0, 0);
            rounds4!(w1, 1);
            rounds4!(w2, 2);
            rounds4!(w3, 3);
            w4 = schedule!(w0, w1, w2, w3);
            rounds4!(w4, 4);
            w0 = schedule!(w1, w2, w3, w4);
            rounds4!(w0, 5);
            w1 = schedule!(w2, w3, w4, w0);
            rounds4!(w1, 6);
            w2 = schedule!(w3, w4, w0, w1);
            rounds4!(w2, 7);
            w3 = schedule!(w4, w0, w1, w2);
            rounds4!(w3, 8);
            w4 = schedule!(w0, w1, w2, w3);
            rounds4!(w4, 9);
            w0 = schedule!(w1, w2, w3, w4);
            rounds4!(w0, 10);
            w1 = schedule!(w2, w3, w4, w0);
            rounds4!(w1, 11);
            w2 = schedule!(w3, w4, w0, w1);
            rounds4!(w2, 12);
            w3 = schedule!(w4, w0, w1, w2);
            rounds4!(w3, 13);
            w4 = schedule!(w0, w1, w2, w3);
            rounds4!(w4, 14);
            w0 = schedule!(w1, w2, w3, w4);
            rounds4!(w0, 15);
            let _ = (w0, w1, w2, w3, w4);
            abef = _mm_add_epi32(abef, abef_save);
            cdgh = _mm_add_epi32(cdgh, cdgh_save);
        }

        let feba = _mm_shuffle_epi32(abef, 0x1b);
        let dchg = _mm_shuffle_epi32(cdgh, 0xb1);
        let dcba = _mm_blend_epi16(feba, dchg, 0xf0);
        let hgef = _mm_alignr_epi8(dchg, feba, 8);
        let sp = state.as_mut_ptr() as *mut __m128i;
        _mm_storeu_si128(sp, dcba);
        _mm_storeu_si128(sp.add(1), hgef);
    }

    /// SHA-1 over whole 64-byte blocks with SHA-NI.
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub unsafe fn sha1_blocks(state: &mut [u32; 5], blocks: &[u8]) {
        let mask = _mm_set_epi64x(0x0001_0203_0405_0607, 0x0809_0a0b_0c0d_0e0f);
        let mut abcd = _mm_set_epi32(
            state[0] as i32,
            state[1] as i32,
            state[2] as i32,
            state[3] as i32,
        );
        let mut e0 = _mm_set_epi32(state[4] as i32, 0, 0, 0);

        macro_rules! rounds4 {
            ($h0:expr, $h1:expr, $wk:expr, $i:expr) => {
                _mm_sha1rnds4_epu32($h0, _mm_sha1nexte_epu32($h1, $wk), $i)
            };
        }
        macro_rules! schedule {
            ($v0:expr, $v1:expr, $v2:expr, $v3:expr) => {
                _mm_sha1msg2_epu32(_mm_xor_si128(_mm_sha1msg1_epu32($v0, $v1), $v2), $v3)
            };
        }

        for b in blocks.chunks_exact(64) {
            let dp = b.as_ptr() as *const __m128i;
            let mut w0 = _mm_shuffle_epi8(_mm_loadu_si128(dp), mask);
            let mut w1 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(1)), mask);
            let mut w2 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(2)), mask);
            let mut w3 = _mm_shuffle_epi8(_mm_loadu_si128(dp.add(3)), mask);
            let mut w4;

            let mut h0 = abcd;
            let mut h1 = _mm_add_epi32(e0, w0);

            // Rounds 0..20
            h1 = _mm_sha1rnds4_epu32(h0, h1, 0);
            h0 = rounds4!(h1, h0, w1, 0);
            h1 = rounds4!(h0, h1, w2, 0);
            h0 = rounds4!(h1, h0, w3, 0);
            w4 = schedule!(w0, w1, w2, w3);
            h1 = rounds4!(h0, h1, w4, 0);
            // Rounds 20..40
            w0 = schedule!(w1, w2, w3, w4);
            h0 = rounds4!(h1, h0, w0, 1);
            w1 = schedule!(w2, w3, w4, w0);
            h1 = rounds4!(h0, h1, w1, 1);
            w2 = schedule!(w3, w4, w0, w1);
            h0 = rounds4!(h1, h0, w2, 1);
            w3 = schedule!(w4, w0, w1, w2);
            h1 = rounds4!(h0, h1, w3, 1);
            w4 = schedule!(w0, w1, w2, w3);
            h0 = rounds4!(h1, h0, w4, 1);
            // Rounds 40..60
            w0 = schedule!(w1, w2, w3, w4);
            h1 = rounds4!(h0, h1, w0, 2);
            w1 = schedule!(w2, w3, w4, w0);
            h0 = rounds4!(h1, h0, w1, 2);
            w2 = schedule!(w3, w4, w0, w1);
            h1 = rounds4!(h0, h1, w2, 2);
            w3 = schedule!(w4, w0, w1, w2);
            h0 = rounds4!(h1, h0, w3, 2);
            w4 = schedule!(w0, w1, w2, w3);
            h1 = rounds4!(h0, h1, w4, 2);
            // Rounds 60..80
            w0 = schedule!(w1, w2, w3, w4);
            h0 = rounds4!(h1, h0, w0, 3);
            w1 = schedule!(w2, w3, w4, w0);
            h1 = rounds4!(h0, h1, w1, 3);
            w2 = schedule!(w3, w4, w0, w1);
            h0 = rounds4!(h1, h0, w2, 3);
            w3 = schedule!(w4, w0, w1, w2);
            h1 = rounds4!(h0, h1, w3, 3);
            w4 = schedule!(w0, w1, w2, w3);
            h0 = rounds4!(h1, h0, w4, 3);
            let _ = (w0, w1, w2, w3, w4);

            abcd = _mm_add_epi32(abcd, h0);
            e0 = _mm_sha1nexte_epu32(h1, e0);
        }

        state[0] = _mm_extract_epi32(abcd, 3) as u32;
        state[1] = _mm_extract_epi32(abcd, 2) as u32;
        state[2] = _mm_extract_epi32(abcd, 1) as u32;
        state[3] = _mm_extract_epi32(abcd, 0) as u32;
        state[4] = _mm_extract_epi32(e0, 3) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn d(algo: Algo, data: &[u8]) -> String {
        hex(&digest(algo, data))
    }

    #[test]
    fn known_answers() {
        assert_eq!(d(Algo::Md5, b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(d(Algo::Md5, b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            d(Algo::Md5, b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        assert_eq!(d(Algo::Sha1, b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            d(Algo::Sha1, b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            d(Algo::Sha256, b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            d(Algo::Sha256, b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            d(Algo::Sha224, b"abc"),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(
            d(Algo::Sha384, b"abc"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            d(Algo::Sha512, b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            d(Algo::Sha512_224, b"abc"),
            "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa"
        );
        assert_eq!(
            d(Algo::Sha512_256, b"abc"),
            "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(d(Algo::Sha1, &million_a), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
        assert_eq!(
            d(Algo::Sha256, &million_a),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The SHA-NI kernels agree with the portable code on every length around block edges.
    #[test]
    fn accelerated_matches_portable() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + i / 3) as u8).collect();
        for n in 0..300 {
            let blocks_len = n / 64 * 64;
            let mut a = [1u32, 2, 3, 4, 5, 6, 7, 8];
            let mut b = a;
            sha256_compress(&mut a, &data[..blocks_len]);
            sha256_compress_soft(&mut b, &data[..blocks_len]);
            assert_eq!(a, b, "sha256 {n}");
            let mut a = [9u32, 8, 7, 6, 5];
            let mut b = a;
            sha1_compress(&mut a, &data[..blocks_len]);
            sha1_compress_soft(&mut b, &data[..blocks_len]);
            assert_eq!(a, b, "sha1 {n}");
        }
    }

    #[test]
    fn streaming_equals_one_shot() {
        let data: Vec<u8> = (0..777u32).map(|i| (i * 13) as u8).collect();
        for algo in [Algo::Md5, Algo::Sha1, Algo::Sha256, Algo::Sha384, Algo::Sha512] {
            let want = digest(algo, &data);
            for split in [0, 1, 63, 64, 65, 127, 128, 129, 500, 777] {
                let mut h = Hasher::new(algo);
                h.update(&data[..split]);
                let copy = h.clone();
                h.update(&data[split..]);
                assert_eq!(h.finish(), want, "{algo:?} split {split}");
                let mut c = copy;
                c.update(&data[split..]);
                assert_eq!(c.finish(), want);
            }
        }
    }

    #[test]
    fn hmac_rfc4231() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac(Algo::Sha256, b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac(Algo::Sha512, b"Jefe", b"what do ya want for nothing?")),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea2505549758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
        // Key longer than the block: hashed first (RFC 4231 test case 6).
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac(Algo::Sha256, &key, b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            hex(&hmac(Algo::Md5, b"key", b"The quick brown fox jumps over the lazy dog")),
            "80070713463e7749b90c2dc24911e275"
        );
    }

    #[test]
    fn pbkdf2_rfc6070() {
        assert_eq!(
            hex(&pbkdf2(Algo::Sha1, b"password", b"salt", 1, 20)),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            hex(&pbkdf2(Algo::Sha1, b"password", b"salt", 4096, 20)),
            "4b007901b765489abead49d926f721d065a429c1"
        );
        assert_eq!(
            hex(&pbkdf2(Algo::Sha1, b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, 25)),
            "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038"
        );
        assert_eq!(
            hex(&pbkdf2(Algo::Sha256, b"password", b"salt", 1, 32)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn hkdf_rfc5869() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0..=12).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        assert_eq!(
            hex(&hkdf(Algo::Sha256, &ikm, &salt, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }
}
