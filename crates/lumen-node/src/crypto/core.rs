//! Digests, HMAC, KDFs and randomness.

use lumen::embed::{JsArrayBuffer, OpDesc, OpError, SendError};

use crate::hash::{self, Algo, Hasher, Hmac};

fn algo(name: &str) -> Result<Algo, OpError> {
    Algo::from_name(name).ok_or_else(|| OpError::error("Digest method not supported"))
}

fn send_algo(name: &str) -> Result<Algo, SendError> {
    Algo::from_name(name).ok_or_else(|| SendError::new("Error", "Digest method not supported"))
}

/// `[outputLength, blockSize, isXof]` of a digest name, or `null` when unknown.
#[lumen::op(name = "hashInfo")]
fn hash_info(name: &str) -> Option<(u32, u32, bool)> {
    Algo::from_name(name).map(|a| (a.out_len() as u32, a.block_len() as u32, a.is_xof()))
}

#[lumen::op]
fn digest(name: &str, data: &[u8]) -> Result<Vec<u8>, OpError> {
    Ok(hash::digest(algo(name)?, data))
}

#[lumen::op(async, name = "digestAsync")]
fn digest_async(name: String, data: Vec<u8>) -> Result<JsArrayBuffer, SendError> {
    Ok(JsArrayBuffer(hash::digest(send_algo(&name)?, &data)))
}

/// A streaming digest.
#[lumen::class(name = "CryptoHash")]
pub struct CryptoHash {
    h: Option<Hasher>,
    xof_len: usize,
}

#[lumen::methods]
impl CryptoHash {
    fn update(&mut self, data: &[u8]) -> Result<(), OpError> {
        self.h.as_mut().ok_or_else(finalized)?.update(data);
        Ok(())
    }

    fn digest(&mut self) -> Result<Vec<u8>, OpError> {
        let len = self.xof_len;
        Ok(self.h.take().ok_or_else(finalized)?.finish_len(len))
    }

    fn copy(&self, xof_len: Option<u32>) -> Result<CryptoHash, OpError> {
        let h = self.h.clone().ok_or_else(finalized)?;
        Ok(CryptoHash { xof_len: xof_len.map_or(self.xof_len, |l| l as usize), h: Some(h) })
    }
}

fn finalized() -> OpError {
    OpError::error("Digest already called").with_code("ERR_CRYPTO_HASH_FINALIZED")
}

/// `new Hash(name, xofLen)`; throws `Digest method not supported` for an unknown name.
#[lumen::op(name = "hashNew")]
fn hash_new(name: &str, xof_len: Option<u32>) -> Result<CryptoHash, OpError> {
    let a = algo(name)?;
    let len = match xof_len {
        Some(l) if !a.is_xof() && l as usize != a.out_len() => {
            return Err(OpError::error("error:030000B2:digital envelope routines::not XOF or invalid length")
                .with_code("ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH"));
        }
        Some(l) => l as usize,
        None => a.out_len(),
    };
    Ok(CryptoHash { h: Some(Hasher::new(a)), xof_len: len })
}

#[lumen::class(name = "CryptoHmac")]
pub struct CryptoHmac {
    h: Option<Hmac>,
}

#[lumen::methods]
impl CryptoHmac {
    fn update(&mut self, data: &[u8]) -> Result<(), OpError> {
        if let Some(h) = self.h.as_mut() {
            h.update(data);
        }
        Ok(())
    }

    /// The MAC; an empty array once it has been taken.
    fn digest(&mut self) -> Vec<u8> {
        self.h.take().map(Hmac::finish).unwrap_or_default()
    }
}

#[lumen::op(name = "hmacNew")]
fn hmac_new(name: &str, key: &[u8]) -> Result<CryptoHmac, OpError> {
    let a = Algo::from_name(name)
        .filter(|a| hash::supports_mac(*a))
        .ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    Ok(CryptoHmac { h: Hmac::new(a, key) })
}

#[lumen::op]
fn hmac(name: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, OpError> {
    let a = Algo::from_name(name)
        .filter(|a| hash::supports_mac(*a))
        .ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    Ok(hash::hmac(a, key, data))
}

fn mac_algo(name: &str) -> Option<Algo> {
    Algo::from_name(name).filter(|a| hash::supports_mac(*a))
}

#[lumen::op]
fn pbkdf2(name: &str, password: &[u8], salt: &[u8], iterations: u32, keylen: u32) -> Result<Vec<u8>, OpError> {
    let a = mac_algo(name).ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    Ok(hash::pbkdf2(a, password, salt, iterations, keylen as usize))
}

#[lumen::op(async, name = "pbkdf2Async")]
fn pbkdf2_async(name: String, password: Vec<u8>, salt: Vec<u8>, iterations: u32, keylen: u32) -> Result<Vec<u8>, SendError> {
    let a = mac_algo(&name).ok_or_else(|| SendError::new("TypeError", format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    Ok(hash::pbkdf2(a, &password, &salt, iterations, keylen as usize))
}

#[lumen::op]
fn hkdf(name: &str, ikm: &[u8], salt: &[u8], info: &[u8], keylen: u32) -> Result<Vec<u8>, OpError> {
    let a = mac_algo(name).ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    if keylen as usize > 255 * a.out_len() {
        return Err(OpError::range_error("Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    Ok(hash::hkdf(a, ikm, salt, info, keylen as usize))
}

#[lumen::op(async, name = "hkdfAsync")]
fn hkdf_async(name: String, ikm: Vec<u8>, salt: Vec<u8>, info: Vec<u8>, keylen: u32) -> Result<Vec<u8>, SendError> {
    let a = mac_algo(&name).ok_or_else(|| SendError::new("TypeError", format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))?;
    if keylen as usize > 255 * a.out_len() {
        return Err(SendError::new("RangeError", "Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    Ok(hash::hkdf(a, &ikm, &salt, &info, keylen as usize))
}

#[lumen::op]
fn scrypt(password: &[u8], salt: &[u8], n: f64, r: u32, p: u32, keylen: u32) -> Result<Vec<u8>, OpError> {
    hash::scrypt(password, salt, n as u64, r, p, keylen as usize).map_err(OpError::error)
}

#[lumen::op(async, name = "scryptAsync")]
fn scrypt_async(password: Vec<u8>, salt: Vec<u8>, n: f64, r: u32, p: u32, keylen: u32) -> Result<Vec<u8>, SendError> {
    hash::scrypt(&password, &salt, n as u64, r, p, keylen as usize).map_err(|e| SendError::new("Error", e))
}

/// Whether scrypt accepts these parameters (N a power of two above one, memory under `maxmem`).
#[lumen::op(name = "scryptCheck")]
fn scrypt_check(n: f64, r: u32, p: u32, maxmem: f64) -> bool {
    let n = n as u64;
    if n < 2 || !n.is_power_of_two() || r == 0 || p == 0 {
        return false;
    }
    let mem = 128u128 * n as u128 * r as u128 + 128u128 * r as u128 * p as u128;
    mem <= maxmem as u128 && ::scrypt::Params::new(n.trailing_zeros() as u8, r, p, 64).is_ok()
}

/// Fill a view with CSPRNG bytes.
#[lumen::op(name = "randomFill")]
fn random_fill(buf: &mut [u8]) -> Result<(), OpError> {
    getrandom::getrandom(buf).map_err(|e| OpError::error(format!("random source failed: {e}")))
}

#[lumen::op(name = "timingSafeEqual")]
fn timing_safe_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

pub const OPS: &[&OpDesc] = lumen::ops![
    hash_info,
    digest,
    digest_async,
    hash_new,
    hmac_new,
    hmac,
    pbkdf2,
    pbkdf2_async,
    hkdf,
    hkdf_async,
    scrypt,
    scrypt_async,
    scrypt_check,
    random_fill,
    timing_safe_equal,
];
