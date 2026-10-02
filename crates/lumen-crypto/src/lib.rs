//! Asymmetric cryptography and bignum operations behind one backend trait.
//!
//! [`backend`] returns the process-wide [`Backend`]: the system OpenSSL when it can be loaded
//! (see `lumen_os::dynlib`, the loader `lumen-tls` shares), RustCrypto otherwise. Setting
//! `LUMEN_CRYPTO_BACKEND=openssl` or `=rustcrypto` forces one; forcing OpenSSL on a machine that
//! has none panics, so tests cannot silently run on the wrong backend.
//!
//! Keys and results cross the trait as big-endian byte strings (no leading-zero requirement on
//! input; outputs are minimal unless documented as padded). Digests are
//! [`lumen_common::hash::Algo`] values and callers hash their own messages: the trait signs and
//! verifies prehashed input, like `EVP_PKEY_sign` does. Errors are [`CryptoError`]s; the OpenSSL
//! backend reports OpenSSL's own error codes and reason strings, the RustCrypto backend imitates
//! the common ones.

use std::sync::OnceLock;

pub use lumen_common::hash::Algo;

mod error;
#[cfg(all(unix, not(target_os = "android")))]
mod openssl;
mod rustcrypto;
mod types;

pub use error::{CryptoError, ErrorKind, Result};
pub use rustcrypto::SysRng;
pub use types::*;

/// One implementation of the operations. All methods are stateless and callable from any thread.
pub trait Backend: Send + Sync {
    /// `"openssl"` or `"rustcrypto"`.
    fn name(&self) -> &'static str;

    /// `publicEncrypt`: RSA encryption with PKCS#1 v1.5, OAEP or no padding (`input` as long as
    /// the modulus).
    fn rsa_encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>>;
    /// `privateDecrypt`.
    fn rsa_decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>>;
    /// `privateEncrypt`: the private-key operation over a PKCS#1 v1.5 (block type 1, no
    /// `DigestInfo`) or raw block. OAEP is refused.
    fn rsa_private_encrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>>;
    /// `publicDecrypt`: recovers the message of [`Backend::rsa_private_encrypt`].
    fn rsa_public_decrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>>;
    /// Signs the digest `hashed` (computed with the scheme's message digest).
    fn rsa_sign(&self, key: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>>;
    /// Whether `signature` is valid for the digest `hashed`; malformed signatures are `false`.
    fn rsa_verify(&self, key: &RsaPublicKey, scheme: &RsaScheme, hashed: &[u8], signature: &[u8]) -> Result<bool>;
    /// A key of `bits` bits with the public exponent `e`.
    fn rsa_generate(&self, bits: u32, e: &[u8]) -> Result<RsaPrivateKey>;

    /// DER `Dss-Sig-Value` over the digest `hashed`.
    fn dsa_sign(&self, key: &DsaPrivateKey, hashed: &[u8]) -> Result<Vec<u8>>;
    /// Verifies a DER `Dss-Sig-Value`; malformed signatures are `false`.
    fn dsa_verify(&self, key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Result<bool>;
    /// Domain parameters of `bits` (`l`) and `divisor_bits` (`n`, backend default when `None`) and
    /// a key pair over them.
    fn dsa_generate(&self, bits: u32, divisor_bits: Option<u32>) -> Result<DsaPrivateKey>;

    /// `DH_check`: the `DH_CHECK_*` flags of the parameters.
    fn dh_check(&self, params: &DhParams) -> Result<u32>;
    /// `DH_check_pub_key`: the `DH_CHECK_PUBKEY_*` flags of a peer value.
    fn dh_check_public(&self, params: &DhParams, public: &[u8]) -> Result<u32>;
    /// A safe prime of `bits` bits that `generator` (2 or 5) generates a subgroup of.
    fn dh_generate_prime(&self, bits: u32, generator: u32) -> Result<Vec<u8>>;
    /// A key pair `(private, public)`; a given `private` only has its public half computed.
    fn dh_generate_key(&self, params: &DhParams, private: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)>;
    /// `g^x mod p` (also a DSA public key), in time independent of `x`.
    fn dh_public(&self, params: &DhParams, private: &[u8]) -> Result<Vec<u8>>;
    /// The shared secret, left-padded with zeros to the length of `p`.
    fn dh_compute(&self, params: &DhParams, private: &[u8], peer: &[u8]) -> Result<Vec<u8>>;

    /// A prime of exactly `bits` bits, optionally safe, optionally congruent to `rem` modulo `add`
    /// (`BN_generate_prime_ex`).
    fn prime_generate(&self, bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>>;
    /// Whether `candidate` is prime after `checks` Miller-Rabin rounds (`0`: the library's default).
    fn prime_check(&self, candidate: &[u8], checks: u32) -> Result<bool>;
}

/// The RustCrypto backend, always available.
pub fn rustcrypto() -> &'static dyn Backend {
    static BACKEND: rustcrypto::RustCrypto = rustcrypto::RustCrypto;
    &BACKEND
}

/// The OpenSSL backend, if the system library loads (once; the answer is cached).
pub fn openssl() -> std::result::Result<&'static dyn Backend, &'static str> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        openssl::backend()
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    {
        Err("OpenSSL is not available on this target")
    }
}

/// The backend every operation goes through: chosen once per process.
pub fn backend() -> &'static dyn Backend {
    static SELECTED: OnceLock<&'static dyn Backend> = OnceLock::new();
    *SELECTED.get_or_init(select)
}

fn select() -> &'static dyn Backend {
    let forced = std::env::var("LUMEN_CRYPTO_BACKEND").ok();
    match forced.as_deref() {
        Some("rustcrypto") => rustcrypto(),
        Some("openssl") => match openssl() {
            Ok(backend) => backend,
            Err(reason) => panic!("LUMEN_CRYPTO_BACKEND=openssl, but OpenSSL cannot be used: {reason}"),
        },
        _ => openssl().unwrap_or_else(|_| rustcrypto()),
    }
}

#[cfg(test)]
mod tests;
