//! Password hashing in the formats of `crypt(3)`: MD5-crypt (`$1$`), SHA-256/SHA-512-crypt
//! (`$5$`, `$6$`, Drepper's scheme) and bcrypt (`$2a$`, `$2b$`, `$2x$`, `$2y$`). The traditional
//! DES formats need the system `crypt`. Node's `Bun.password` and Python's `_crypt` both run on
//! this.

use crate::hash::{digest, Algo};

const ITOA64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn to64(out: &mut String, mut v: u32, n: usize) {
    for _ in 0..n {
        out.push(ITOA64[(v & 0x3f) as usize] as char);
        v >>= 6;
    }
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn md5_crypt(password: &[u8], setting: &str) -> Option<String> {
    let rest = setting.strip_prefix("$1$")?;
    let salt = rest.split('$').next().unwrap_or("");
    let salt = &salt.as_bytes()[..salt.len().min(8)];
    let alt = digest(Algo::Md5, &cat(&[password, salt, password]));
    let mut ctx = cat(&[password, b"$1$", salt]);
    let mut left = password.len();
    while left > 0 {
        ctx.extend_from_slice(&alt[..left.min(16)]);
        left = left.saturating_sub(16);
    }
    let mut i = password.len();
    while i > 0 {
        ctx.push(if i & 1 == 1 { 0 } else { password[0] });
        i >>= 1;
    }
    let mut f = digest(Algo::Md5, &ctx);
    for round in 0..1000 {
        let mut c = Vec::new();
        c.extend_from_slice(if round & 1 == 1 { password } else { &f });
        if round % 3 != 0 {
            c.extend_from_slice(salt);
        }
        if round % 7 != 0 {
            c.extend_from_slice(password);
        }
        c.extend_from_slice(if round & 1 == 1 { &f } else { password });
        f = digest(Algo::Md5, &c);
    }
    let mut out = String::from("$1$");
    out.push_str(&String::from_utf8_lossy(salt));
    out.push('$');
    let w = |a: usize, b: usize, c: usize| ((f[a] as u32) << 16) | ((f[b] as u32) << 8) | f[c] as u32;
    for (a, b, c) in [(0, 6, 12), (1, 7, 13), (2, 8, 14), (3, 9, 15), (4, 10, 5)] {
        to64(&mut out, w(a, b, c), 4);
    }
    to64(&mut out, f[11] as u32, 2);
    Some(out)
}

const SHA256_ORDER: [(usize, usize, usize); 10] =
    [(0, 10, 20), (21, 1, 11), (12, 22, 2), (3, 13, 23), (24, 4, 14), (15, 25, 5), (6, 16, 26), (27, 7, 17), (18, 28, 8), (9, 19, 29)];

const SHA512_ORDER: [(usize, usize, usize); 21] = [
    (0, 21, 42),
    (22, 43, 1),
    (44, 2, 23),
    (3, 24, 45),
    (25, 46, 4),
    (47, 5, 26),
    (6, 27, 48),
    (28, 49, 7),
    (50, 8, 29),
    (9, 30, 51),
    (31, 52, 10),
    (53, 11, 32),
    (12, 33, 54),
    (34, 55, 13),
    (56, 14, 35),
    (15, 36, 57),
    (37, 58, 16),
    (59, 17, 38),
    (18, 39, 60),
    (40, 61, 19),
    (62, 20, 41),
];

fn repeat_to(block: &[u8], len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let n = block.len().min(len - v.len());
        v.extend_from_slice(&block[..n]);
    }
    v
}

fn sha_crypt(password: &[u8], setting: &str, wide: bool) -> Option<String> {
    let (prefix, algo) = if wide { ("$6$", Algo::Sha512) } else { ("$5$", Algo::Sha256) };
    let mut rest = setting.strip_prefix(prefix)?;
    let mut rounds = 5000usize;
    let mut custom = false;
    if let Some(r) = rest.strip_prefix("rounds=") {
        if let Some((n, tail)) = r.split_once('$') {
            if let Ok(n) = n.parse::<u64>() {
                rounds = n.clamp(1000, 999_999_999) as usize;
                custom = true;
                rest = tail;
            }
        }
    }
    let salt = rest.split('$').next().unwrap_or("");
    let salt = &salt.as_bytes()[..salt.len().min(16)];
    let hlen = algo.out_len();
    let b = digest(algo, &cat(&[password, salt, password]));
    let mut ctx = cat(&[password, salt]);
    let mut cnt = password.len();
    while cnt > hlen {
        ctx.extend_from_slice(&b);
        cnt -= hlen;
    }
    ctx.extend_from_slice(&b[..cnt]);
    let mut cnt = password.len();
    while cnt > 0 {
        ctx.extend_from_slice(if cnt & 1 == 1 { &b } else { password });
        cnt >>= 1;
    }
    let a = digest(algo, &ctx);
    let dp = digest(algo, &password.repeat(password.len()));
    let p = repeat_to(&dp, password.len());
    let ds = digest(algo, &salt.repeat(16 + a[0] as usize));
    let s = repeat_to(&ds, salt.len());
    let mut c = a;
    for r in 0..rounds {
        let mut x = Vec::new();
        x.extend_from_slice(if r & 1 == 1 { &p } else { &c });
        if r % 3 != 0 {
            x.extend_from_slice(&s);
        }
        if r % 7 != 0 {
            x.extend_from_slice(&p);
        }
        x.extend_from_slice(if r & 1 == 1 { &c } else { &p });
        c = digest(algo, &x);
    }
    let mut out = String::from(prefix);
    if custom {
        out.push_str(&format!("rounds={rounds}$"));
    }
    out.push_str(&String::from_utf8_lossy(salt));
    out.push('$');
    let w = |a: usize, b: usize, c2: usize| ((c[a] as u32) << 16) | ((c[b] as u32) << 8) | c[c2] as u32;
    if wide {
        for (a, b, c2) in SHA512_ORDER {
            to64(&mut out, w(a, b, c2), 4);
        }
        to64(&mut out, c[63] as u32, 2);
    } else {
        for (a, b, c2) in SHA256_ORDER {
            to64(&mut out, w(a, b, c2), 4);
        }
        to64(&mut out, ((c[31] as u32) << 8) | c[30] as u32, 3);
    }
    Some(out)
}

/// bcrypt over `password` as the key (at most 72 bytes after the terminating NUL): the 23-byte
/// digest. Callers that pre-hash long passwords do so before calling.
pub fn bcrypt_raw(password: &[u8], salt: &[u8; 16], cost: u32) -> [u8; 23] {
    let mut key = Vec::with_capacity(73);
    key.extend_from_slice(password);
    key.push(0);
    key.truncate(72);
    let out = bcrypt::bcrypt(cost, *salt, &key);
    let mut digest = [0u8; 23];
    digest.copy_from_slice(&out[..23]);
    digest
}

const STD_B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BCRYPT_B64: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn translate(text: &[u8], from: &[u8; 64], to: &[u8; 64]) -> Option<Vec<u8>> {
    text.iter().map(|c| from.iter().position(|f| f == c).map(|i| to[i])).collect()
}

/// bcrypt's base-64 text of `bytes` (no padding).
pub fn bcrypt_encode(bytes: &[u8]) -> String {
    let std = crate::codec::base64_encode(bytes, false, false);
    let out = translate(std.as_bytes(), STD_B64, BCRYPT_B64).unwrap_or_default();
    String::from_utf8(out).unwrap_or_default()
}

/// The bytes of bcrypt base-64 `text`.
pub fn bcrypt_decode(text: &str) -> Option<Vec<u8>> {
    let mut std = translate(text.as_bytes(), BCRYPT_B64, STD_B64)?;
    let clear = match std.len() % 4 {
        2 => 0xf,
        3 => 0x3,
        _ => 0,
    };
    if let Some(last) = std.last_mut() {
        let i = STD_B64.iter().position(|c| c == last)?;
        *last = STD_B64[i & !clear];
    }
    crate::codec::base64_decode_strict(&std, false, crate::codec::Padding::Forbidden).ok()
}

/// `$2b$NN$<22 salt chars><31 digest chars>` with the given minor version letter.
pub fn bcrypt_string(version: char, password: &[u8], salt: &[u8; 16], cost: u32) -> String {
    let digest = bcrypt_raw(password, salt, cost);
    format!("$2{version}${cost:02}${}{}", bcrypt_encode(salt), bcrypt_encode(&digest))
}

fn bcrypt_crypt(password: &[u8], setting: &str) -> Option<String> {
    let b = setting.as_bytes();
    if b.len() < 29 || b[0] != b'$' || b[1] != b'2' || b[3] != b'$' || b[6] != b'$' || !setting.is_ascii() {
        return None;
    }
    let version = b[2] as char;
    if !matches!(version, 'a' | 'b' | 'x' | 'y') {
        return None;
    }
    let cost: u32 = setting[4..6].parse().ok()?;
    if !(4..=31).contains(&cost) {
        return None;
    }
    let salt: [u8; 16] = bcrypt_decode(&setting[7..29])?.try_into().ok()?;
    Some(bcrypt_string(version, password, &salt, cost))
}

/// `crypt(3)` for the formats implemented here; `None` when `setting` names another format or
/// is malformed.
pub fn crypt(password: &[u8], setting: &str) -> Option<String> {
    if setting.starts_with("$1$") {
        md5_crypt(password, setting)
    } else if setting.starts_with("$5$") {
        sha_crypt(password, setting, false)
    } else if setting.starts_with("$6$") {
        sha_crypt(password, setting, true)
    } else if setting.starts_with("$2") {
        bcrypt_crypt(password, setting)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_crypt_reference_vectors() {
        assert_eq!(
            crypt(b"Hello world!", "$5$saltstring").unwrap(),
            "$5$saltstring$5B8vYYiY.CVt1RlTTf8KbXBH3hsxY/GNooZaBBGWEc5"
        );
        assert_eq!(
            crypt(b"Hello world!", "$6$saltstring").unwrap(),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
    }

    #[test]
    fn bcrypt_round_trips_its_own_output() {
        let h = crypt(b"secret", "$2b$04$abcdefghijklmnopqrstuu").unwrap();
        assert_eq!(h.len(), 60);
        assert_eq!(crypt(b"secret", &h).unwrap(), h);
    }

    #[test]
    fn md5_crypt_is_stable_and_salted() {
        let h = crypt(b"pw", "$1$saltsalt$").unwrap();
        assert!(h.starts_with("$1$saltsalt$") && h.len() == 12 + 22);
        assert_eq!(crypt(b"pw", &h).unwrap(), h);
    }
}
