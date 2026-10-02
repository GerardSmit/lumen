//! Asymmetric cryptography and bignum operations behind one backend trait.
//!
//! The operating system's own cryptography comes first, so the OS vendor's security updates reach
//! Lumen without a rebuild; RustCrypto is there only where no system crypto exists.
//!
//! | backend      | feature      | targets                     | what it is                              |
//! |--------------|--------------|-----------------------------|-----------------------------------------|
//! | `apple`      | `apple`      | macOS, iOS and the like     | Security.framework (`SecKey`)           |
//! | `cng`        | `cng`        | Windows                     | `bcrypt.dll`                            |
//! | `openssl`    | `openssl`    | unix except Android         | system OpenSSL 3 through `dlopen`       |
//! | `rustcrypto` | `rustcrypto` | everywhere                  | `rsa`, `dsa`, `crypto-bigint`, ...      |
//!
//! All four features are on by default and each is a no-op on a target that cannot host it, so the
//! defaults are: Linux openssl + rustcrypto, Apple apple + openssl + rustcrypto, Windows cng +
//! rustcrypto, Android and wasm rustcrypto. `--no-default-features --features apple` (or `cng`,
//! `openssl`) builds without the RustCrypto asymmetric crates; the operations the system backend
//! cannot do then fail with Node's `ERR_CRYPTO_UNSUPPORTED_OPERATION`.
//!
//! [`backend`] returns the process-wide [`Backend`]: the platform's system backend, else OpenSSL
//! when it loads, else RustCrypto (so on macOS OpenSSL is opt-in, for Node-exact behaviour).
//! `LUMEN_CRYPTO_BACKEND=apple|cng|openssl|rustcrypto` forces one; forcing one that is not
//! compiled in or cannot be loaded panics, so tests cannot silently run on the wrong backend.
//!
//! The system backends share one layer (`system`) over a few RSA primitives; each backend's module
//! documents which operations are native, which are built on the native raw operation and which
//! go to the fallback.
//!
//! Keys and results cross the trait as big-endian byte strings (no leading-zero requirement on
//! input; outputs are minimal unless documented as padded). Digests are
//! [`lumen_common::hash::Algo`] values and callers hash their own messages: the trait signs and
//! verifies prehashed input, like `EVP_PKEY_sign` does. Errors are [`CryptoError`]s; the OpenSSL
//! backend reports OpenSSL's own error codes and reason strings, the others imitate the common
//! ones (decrypt failure, data too large, bad key, unsupported).

use std::sync::OnceLock;

pub use lumen_common::hash::Algo;

#[cfg(crypto_apple)]
mod apple;
#[cfg(crypto_cng)]
mod cng;
mod error;
#[cfg(any(crypto_apple, crypto_cng))]
mod der;
#[cfg(crypto_openssl)]
mod openssl;
#[cfg(any(crypto_rustcrypto, crypto_apple, crypto_cng))]
#[allow(dead_code)] // which items a build uses depends on the backends it contains
mod pss;
mod rng;
#[allow(dead_code)] // which items a build uses depends on the backends it contains
mod rsa_util;
#[cfg(crypto_rustcrypto)]
mod rustcrypto;
#[cfg(any(crypto_apple, crypto_cng))]
mod system;
mod types;

pub use error::{CryptoError, ErrorKind, Result};
pub use rng::SysRng;
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

/// A backend that is not compiled in, or cannot be used on this machine, is an `Err` with the reason.
pub type BackendResult = std::result::Result<&'static dyn Backend, &'static str>;

/// The RustCrypto backend (the `rustcrypto` feature).
pub fn rustcrypto() -> BackendResult {
    #[cfg(crypto_rustcrypto)]
    {
        static BACKEND: rustcrypto::RustCrypto = rustcrypto::RustCrypto;
        Ok(&BACKEND)
    }
    #[cfg(not(crypto_rustcrypto))]
    {
        Err("the rustcrypto backend is not compiled in")
    }
}

/// The OpenSSL backend, if it is compiled in and the system library loads (once; the answer is
/// cached).
pub fn openssl() -> BackendResult {
    #[cfg(crypto_openssl)]
    {
        openssl::backend()
    }
    #[cfg(not(crypto_openssl))]
    {
        Err("the openssl backend is not compiled in")
    }
}

/// The Security.framework backend (the `apple` feature, Apple targets).
pub fn apple() -> BackendResult {
    #[cfg(crypto_apple)]
    {
        Ok(apple::backend())
    }
    #[cfg(not(crypto_apple))]
    {
        Err("the apple backend is not compiled in")
    }
}

/// The Windows CNG backend (the `cng` feature, Windows targets).
pub fn cng() -> BackendResult {
    #[cfg(crypto_cng)]
    {
        Ok(cng::backend())
    }
    #[cfg(not(crypto_cng))]
    {
        Err("the cng backend is not compiled in")
    }
}

/// Every backend this build contains and this machine can use, in selection order.
pub fn backends() -> Vec<&'static dyn Backend> {
    [apple(), cng(), openssl(), rustcrypto()].into_iter().flatten().collect()
}

/// The backend every operation goes through: chosen once per process.
pub fn backend() -> &'static dyn Backend {
    static SELECTED: OnceLock<&'static dyn Backend> = OnceLock::new();
    *SELECTED.get_or_init(select)
}

fn select() -> &'static dyn Backend {
    let forced = std::env::var("LUMEN_CRYPTO_BACKEND").ok();
    let choice = match forced.as_deref() {
        Some("apple") => Some(apple()),
        Some("cng") => Some(cng()),
        Some("openssl") => Some(openssl()),
        Some("rustcrypto") => Some(rustcrypto()),
        _ => None,
    };
    match choice {
        Some(Ok(backend)) => backend,
        Some(Err(reason)) => panic!("LUMEN_CRYPTO_BACKEND={}, but that backend cannot be used: {reason}", forced.unwrap_or_default()),
        None => backends().first().copied().expect("the build contains a crypto backend"),
    }
}

#[cfg(test)]
mod tests;
