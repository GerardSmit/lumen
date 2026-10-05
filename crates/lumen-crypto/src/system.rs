//! The logic the operating system's backends (Security.framework, CNG) share. A system backend
//! supplies [`Primitives`], the handful of RSA operations its API has; [`System`] turns them into
//! a [`Backend`]: it checks inputs the way OpenSSL reports them, builds the `DigestInfo` blocks,
//! runs PSS over the raw operation where the native one cannot express the request, undoes
//! block-type-1 padding, and hands everything the system has no API for (DSA, finite-field DH,
//! primes, rejected keys, unusual parameters) to the fallback backend, or reports Node's
//! "unsupported operation" error without one.

use std::cell::Cell;

use crate::error::{CryptoError, Result};
use crate::rsa_util::{
    be_ge, bit_len, check_block_len, data_too_large, decoder_unsupported, digest_info, digest_too_big, illegal_padding, mod_len,
    oaep_decoding, padding_check_failed, sign_failed, unpad_type1,
};
use crate::{pad_be, pss, Algo, Backend, DhParams, DsaPrivateKey, DsaPublicKey, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey, RsaScheme};

pub(crate) type Fallback = Option<&'static dyn Backend>;

/// Why a native operation did not produce a result.
pub(crate) enum NativeError {
    /// The system refused to import the key (too small, inconsistent): the fallback may accept it.
    Rejected,
    Failed(CryptoError),
}

impl From<CryptoError> for NativeError {
    fn from(error: CryptoError) -> NativeError {
        NativeError::Failed(error)
    }
}

pub(crate) type Native<T> = std::result::Result<T, NativeError>;

/// The operations of a system RSA API. [`System`] has checked the inputs; the arguments of the raw
/// operations are as long as the modulus.
pub(crate) trait Primitives: Send + Sync + 'static {
    const NAME: &'static str;

    /// Whether OAEP with these parameters can be done natively (PKCS#1 v1.5 and raw always can).
    fn oaep(&self, hash: Algo, mgf1: Algo, label: &[u8]) -> bool;
    /// PKCS#1 v1.5 or native OAEP encryption.
    fn encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>>;
    fn decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>>;
    /// `block^e mod n`.
    fn raw_public(&self, key: &RsaPublicKey, block: &[u8]) -> Native<Vec<u8>>;
    /// `block^d mod n`, unchecked.
    fn raw_private(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>>;
    /// A signature over `block` under PKCS#1 v1.5 block type 1, no digest wrapping.
    fn sign_block(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>>;
    fn verify_block(&self, key: &RsaPublicKey, block: &[u8], signature: &[u8]) -> Native<bool>;
    /// The salt length of the native PSS for the request, if it has one.
    fn pss_salt(&self, hash: Algo, mgf1: Algo, salt: PssSalt, n: &[u8], signing: bool) -> Option<usize>;
    fn pss_sign(&self, key: &RsaPrivateKey, hash: Algo, salt_len: usize, hashed: &[u8]) -> Native<Vec<u8>>;
    fn pss_verify(&self, key: &RsaPublicKey, hash: Algo, salt_len: usize, hashed: &[u8], signature: &[u8]) -> Native<bool>;
    /// A generated key, or `None` for sizes and exponents the system does not do.
    fn generate(&self, bits: u32, e: &[u8]) -> Option<Native<RsaPrivateKey>>;
    fn dsa_sign(&self, _key: &DsaPrivateKey, _hashed: &[u8]) -> Option<Native<Vec<u8>>> {
        None
    }
    fn dsa_verify(&self, _key: &DsaPublicKey, _hashed: &[u8], _signature: &[u8]) -> Option<Native<bool>> {
        None
    }
}

pub(crate) struct System<P: Primitives> {
    native: P,
    fallback: Fallback,
}

impl<P: Primitives> System<P> {
    pub(crate) fn new(native: P, fallback: Fallback) -> System<P> {
        System { native, fallback }
    }

    fn delegate<T>(&self, op: impl FnOnce(&dyn Backend) -> Result<T>) -> Result<T> {
        match self.fallback {
            Some(backend) => op(backend),
            None => Err(CryptoError::unsupported()),
        }
    }

    /// A native result as a plain one; a rejected key goes to the fallback (which accepts small
    /// keys), without one it is Node's decoder error.
    fn resolve<T>(&self, native: Native<T>, fallback: impl FnOnce(&dyn Backend) -> Result<T>) -> Result<T> {
        match native {
            Ok(value) => Ok(value),
            Err(NativeError::Failed(error)) => Err(error),
            Err(NativeError::Rejected) => match self.fallback {
                Some(backend) => fallback(backend),
                None => Err(decoder_unsupported()),
            },
        }
    }

    /// The private-key operation, checked against the public one so a fault cannot leak the key.
    fn raw_private_checked(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>> {
        let k_len = mod_len(&key.public.n);
        let out = pad_be(&self.native.raw_private(key, block)?, k_len);
        let check = pad_be(&self.native.raw_public(&key.public, &out)?, k_len);
        if check == pad_be(block, k_len) {
            Ok(out)
        } else {
            Err(sign_failed().into())
        }
    }

    fn native_padding(&self, padding: &RsaPadding) -> bool {
        match padding {
            RsaPadding::Oaep { hash, mgf1, label } => self.native.oaep(*hash, *mgf1, label),
            _ => true,
        }
    }
}

impl<P: Primitives> Backend for System<P> {
    fn name(&self) -> &'static str {
        P::NAME
    }

    fn rsa_encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        if !self.native_padding(padding) {
            return self.delegate(|b| b.rsa_encrypt(key, padding, input));
        }
        let k_len = mod_len(&key.n);
        let native = match padding {
            RsaPadding::Oaep { hash, .. } => {
                if input.len() + 2 * hash.out_len() + 2 > k_len {
                    return Err(data_too_large());
                }
                self.native.encrypt(key, padding, input)
            }
            RsaPadding::Pkcs1 => {
                if input.len() + 11 > k_len {
                    return Err(data_too_large());
                }
                self.native.encrypt(key, padding, input)
            }
            RsaPadding::None => {
                check_block_len(key, input)?;
                self.native.raw_public(key, input)
            }
        };
        self.resolve(native.map(|out| pad_be(&out, k_len)), |b| b.rsa_encrypt(key, padding, input))
    }

    fn rsa_decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        if !self.native_padding(padding) {
            return self.delegate(|b| b.rsa_decrypt(key, padding, input));
        }
        check_block_len(&key.public, input)?;
        let native = match padding {
            RsaPadding::None => self.raw_private_checked(key, input),
            RsaPadding::Pkcs1 => self.native.decrypt(key, padding, input).map_err(|e| mask(e, padding_check_failed)),
            RsaPadding::Oaep { .. } => self.native.decrypt(key, padding, input).map_err(|e| mask(e, oaep_decoding)),
        };
        self.resolve(native, |b| b.rsa_decrypt(key, padding, input))
    }

    fn rsa_private_encrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        let k_len = mod_len(&key.public.n);
        let native = match padding {
            RsaPadding::Pkcs1 => {
                if input.len() + 11 > k_len {
                    return Err(data_too_large());
                }
                self.native.sign_block(key, input)
            }
            RsaPadding::None => {
                check_block_len(&key.public, input)?;
                self.raw_private_checked(key, input)
            }
            RsaPadding::Oaep { .. } => return Err(illegal_padding()),
        };
        self.resolve(native.map(|out| pad_be(&out, k_len)), |b| b.rsa_private_encrypt(key, padding, input))
    }

    fn rsa_public_decrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        if matches!(padding, RsaPadding::Oaep { .. }) {
            return Err(illegal_padding());
        }
        let k_len = check_block_len(key, input)?;
        let Some(em) = self.native.raw_public(key, input).ok() else {
            return self.delegate(|b| b.rsa_public_decrypt(key, padding, input));
        };
        let em = pad_be(&em, k_len);
        match padding {
            RsaPadding::Pkcs1 => unpad_type1(&em),
            _ => Ok(em),
        }
    }

    fn rsa_sign(&self, key: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>> {
        let k_len = mod_len(&key.public.n);
        match scheme {
            RsaScheme::Pkcs1 { hash } => {
                let block = digest_info(*hash, hashed)?;
                if k_len < block.len() + 11 {
                    return Err(digest_too_big());
                }
                let native = self.native.sign_block(key, &block).map(|out| pad_be(&out, k_len));
                self.resolve(native, |b| b.rsa_sign(key, scheme, hashed))
            }
            RsaScheme::Pss { hash, mgf1, salt } => {
                if hashed.len() == hash.out_len() {
                    if let Some(salt_len) = self.native.pss_salt(*hash, *mgf1, *salt, &key.public.n, true) {
                        let native = self.native.pss_sign(key, *hash, salt_len, hashed).map(|out| pad_be(&out, k_len));
                        return self.resolve(native, |b| b.rsa_sign(key, scheme, hashed));
                    }
                }
                let salt_len = pss::sign_salt_len(&key.public.n, *hash, *salt)?;
                let rejected = Cell::new(false);
                let signed = pss::sign(&key.public.n, *hash, *mgf1, hashed, salt_len, |em| match self.raw_private_checked(key, em) {
                    Ok(out) => Ok(out),
                    Err(NativeError::Failed(error)) => Err(error),
                    Err(NativeError::Rejected) => {
                        rejected.set(true);
                        Err(sign_failed())
                    }
                });
                if rejected.get() {
                    return self.resolve(Err(NativeError::Rejected), |b| b.rsa_sign(key, scheme, hashed));
                }
                signed
            }
        }
    }

    fn rsa_verify(&self, key: &RsaPublicKey, scheme: &RsaScheme, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        if bit_len(&key.n) < 2 || signature.len() != mod_len(&key.n) || be_ge(signature, &key.n) {
            return Ok(false);
        }
        match scheme {
            RsaScheme::Pkcs1 { hash } => {
                let block = digest_info(*hash, hashed)?;
                self.resolve(self.native.verify_block(key, &block, signature), |b| b.rsa_verify(key, scheme, hashed, signature))
            }
            RsaScheme::Pss { hash, mgf1, salt } => {
                if hashed.len() == hash.out_len() {
                    if let Some(salt_len) = self.native.pss_salt(*hash, *mgf1, *salt, &key.n, false) {
                        let native = self.native.pss_verify(key, *hash, salt_len, hashed, signature);
                        return self.resolve(native, |b| b.rsa_verify(key, scheme, hashed, signature));
                    }
                }
                match self.native.raw_public(key, signature) {
                    Ok(em) => Ok(pss::verify(&em, &key.n, *hash, *mgf1, hashed, *salt)),
                    Err(NativeError::Failed(error)) => Err(error),
                    Err(NativeError::Rejected) => self.resolve(Err(NativeError::Rejected), |b| b.rsa_verify(key, scheme, hashed, signature)),
                }
            }
        }
    }

    fn rsa_generate(&self, bits: u32, e: &[u8]) -> Result<RsaPrivateKey> {
        match self.native.generate(bits, e) {
            Some(native) => self.resolve(native, |b| b.rsa_generate(bits, e)),
            None => self.delegate(|b| b.rsa_generate(bits, e)),
        }
    }

    fn dsa_sign(&self, key: &DsaPrivateKey, hashed: &[u8]) -> Result<Vec<u8>> {
        match self.native.dsa_sign(key, hashed) {
            Some(native) => self.resolve(native, |b| b.dsa_sign(key, hashed)),
            None => self.delegate(|b| b.dsa_sign(key, hashed)),
        }
    }

    fn dsa_verify(&self, key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        match self.native.dsa_verify(key, hashed, signature) {
            Some(native) => self.resolve(native, |b| b.dsa_verify(key, hashed, signature)),
            None => self.delegate(|b| b.dsa_verify(key, hashed, signature)),
        }
    }

    fn dsa_generate(&self, bits: u32, divisor_bits: Option<u32>) -> Result<DsaPrivateKey> {
        self.delegate(|b| b.dsa_generate(bits, divisor_bits))
    }

    fn dh_check(&self, params: &DhParams) -> Result<u32> {
        self.delegate(|b| b.dh_check(params))
    }

    fn dh_check_public(&self, params: &DhParams, public: &[u8]) -> Result<u32> {
        self.delegate(|b| b.dh_check_public(params, public))
    }

    fn dh_generate_prime(&self, bits: u32, generator: u32) -> Result<Vec<u8>> {
        self.delegate(|b| b.dh_generate_prime(bits, generator))
    }

    fn dh_generate_key(&self, params: &DhParams, private: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
        self.delegate(|b| b.dh_generate_key(params, private))
    }

    fn dh_public(&self, params: &DhParams, private: &[u8]) -> Result<Vec<u8>> {
        self.delegate(|b| b.dh_public(params, private))
    }

    fn dh_compute(&self, params: &DhParams, private: &[u8], peer: &[u8]) -> Result<Vec<u8>> {
        self.delegate(|b| b.dh_compute(params, private, peer))
    }

    fn prime_generate(&self, bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>> {
        self.delegate(|b| b.prime_generate(bits, safe, add, rem))
    }

    fn prime_check(&self, candidate: &[u8], checks: u32) -> Result<bool> {
        self.delegate(|b| b.prime_check(candidate, checks))
    }
}

/// Every failure of a padding check surfaces as the same error; a rejected key stays rejected.
fn mask(error: NativeError, failure: fn() -> CryptoError) -> NativeError {
    match error {
        NativeError::Rejected => NativeError::Rejected,
        NativeError::Failed(_) => NativeError::Failed(failure()),
    }
}
