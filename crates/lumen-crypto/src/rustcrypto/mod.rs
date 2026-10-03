//! The RustCrypto backend: `rsa`, `dsa`, `crypto-bigint` and `crypto-primes`. Compiled with
//! the `rustcrypto` feature; the only backend on targets without a system one and the
//! per-operation fallback of the system backends.
//!
//! Differences from OpenSSL: finite-field DH needs an odd modulus; PSS refuses `md5-sha1`;
//! `prime_check` ignores the round count; primality of safe primes and generation with `add` /
//! `rem` follow the library's own algorithms; PKCS#1 v1.5 decryption has no implicit rejection;
//! error texts of the common failures are imitated, the rest are generic.

mod bignum;
mod dh;
mod dsa;
mod prime;
mod rsa;


use crate::error::Result;
use crate::{
    Backend, DhParams, DsaPrivateKey, DsaPublicKey, RsaPadding, RsaPrivateKey, RsaPublicKey, RsaScheme,
};

pub(crate) struct RustCrypto;

impl Backend for RustCrypto {
    fn name(&self) -> &'static str {
        "rustcrypto"
    }

    fn rsa_encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        rsa::encrypt(key, padding, input)
    }

    fn rsa_decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        rsa::decrypt(key, padding, input)
    }

    fn rsa_private_encrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        rsa::private_encrypt(key, padding, input)
    }

    fn rsa_public_decrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        rsa::public_decrypt(key, padding, input)
    }

    fn rsa_sign(&self, key: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>> {
        rsa::sign(key, scheme, hashed)
    }

    fn rsa_verify(&self, key: &RsaPublicKey, scheme: &RsaScheme, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        rsa::verify(key, scheme, hashed, signature)
    }

    fn rsa_generate(&self, bits: u32, e: &[u8]) -> Result<RsaPrivateKey> {
        rsa::generate(bits, e)
    }

    fn dsa_sign(&self, key: &DsaPrivateKey, hashed: &[u8]) -> Result<Vec<u8>> {
        dsa::sign(key, hashed)
    }

    fn dsa_verify(&self, key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        dsa::verify(key, hashed, signature)
    }

    fn dsa_generate(&self, bits: u32, divisor_bits: Option<u32>) -> Result<DsaPrivateKey> {
        dsa::generate(bits, divisor_bits)
    }

    fn dh_check(&self, params: &DhParams) -> Result<u32> {
        dh::check(params)
    }

    fn dh_check_public(&self, params: &DhParams, public: &[u8]) -> Result<u32> {
        dh::check_public(params, public)
    }

    fn dh_generate_prime(&self, bits: u32, generator: u32) -> Result<Vec<u8>> {
        dh::generate_prime_for(bits, generator)
    }

    fn dh_generate_key(&self, params: &DhParams, private: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
        dh::generate_key(params, private)
    }

    fn dh_public(&self, params: &DhParams, private: &[u8]) -> Result<Vec<u8>> {
        dh::public(params, private)
    }

    fn dh_compute(&self, params: &DhParams, private: &[u8], peer: &[u8]) -> Result<Vec<u8>> {
        dh::compute(params, private, peer)
    }

    fn prime_generate(&self, bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>> {
        prime::generate(bits, safe, add, rem)
    }

    fn prime_check(&self, candidate: &[u8], _checks: u32) -> Result<bool> {
        prime::check(candidate)
    }
}
