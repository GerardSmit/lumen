//! bcrypt (`$2a$`, `$2b$`, `$2x$`, `$2y$`): the raw digest, bcrypt's base-64 alphabet and the
//! `$2b$NN$salt+digest` text. Node's `Bun.password` runs on this.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcrypt_string_round_trips_its_salt() {
        let salt: [u8; 16] = bcrypt_decode("abcdefghijklmnopqrstuu").unwrap().try_into().unwrap();
        let h = bcrypt_string('b', b"secret", &salt, 4);
        assert_eq!(h.len(), 60);
        assert_eq!(&h[..7], "$2b$04$");
        assert_eq!(bcrypt_encode(&salt), "abcdefghijklmnopqrstuu");
        assert_eq!(h, bcrypt_string('b', b"secret", &salt, 4));
    }
}
