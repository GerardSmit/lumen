//! The Apple backend: RSA through Security.framework's `SecKey`, on the shared
//! [`System`](crate::system::System) logic.
//!
//! Native: RSA PKCS#1 v1.5 and raw encryption, OAEP with an empty label and one digest for the
//! hash and MGF1 (SHA-1, SHA-2), PKCS#1 v1.5 signatures over every digest `lumen_common::hash`
//! has (the `DigestInfo` is built by the shared code and signed as a block), PSS with a salt as
//! long as the digest, the raw private and public operations, and 1024..4096-bit key generation
//! with `e = 65537`.
//!
//! Built on the raw operation (no fallback needed): PSS with any other salt length, an MGF1
//! digest other than the message digest, or automatic salt detection.
//!
//! Handed to the fallback (RustCrypto, when the build has it; otherwise Node's "unsupported
//! operation" error): OAEP with a label, with an MGF1 digest other than the label digest, or with
//! a digest outside SHA-1 / SHA-2; RSA keys Security.framework refuses to import (very small or
//! inconsistent ones) or generate (other sizes, public exponents other than 65537); DSA, finite
//! field Diffie-Hellman, safe-prime generation and primality testing, which Security.framework
//! has no API for. ECDSA and ECDH are outside the backend trait.
//!
//! Randomness is the system generator (`lumen_os::proc::entropy` is `getentropy`, the source
//! `CCRandomGenerateBytes` draws from); the hash functions are `lumen_common::hash`, so
//! CommonCrypto digests are not linked.

mod error;

use std::sync::OnceLock;

use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::error::CFError;
use core_foundation_sys::base::CFTypeRef;
use core_foundation_sys::error::CFErrorRef;
use core_foundation_sys::string::CFStringRef;
use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token};
use security_framework_sys::item::{kSecAttrKeyClass, kSecAttrKeyClassPrivate, kSecAttrKeyClassPublic, kSecAttrKeyType, kSecAttrKeyTypeRSA};
use security_framework_sys::key::SecKeyCreateWithData;

use crate::error::CryptoError;
use crate::system::{Native, NativeError, Primitives, System};
use crate::{der, trim_be, Algo, Backend, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey};
use error::Context;

const PUBLIC_EXPONENT: [u8; 3] = [1, 0, 1];
const GENERATED_BITS: std::ops::RangeInclusive<u32> = 1024..=4096;

struct Apple;

pub(crate) fn backend() -> &'static dyn Backend {
    static BACKEND: OnceLock<System<Apple>> = OnceLock::new();
    BACKEND.get_or_init(|| System::new(Apple, crate::rustcrypto().ok()))
}

fn import(der: &[u8], class: CFStringRef) -> Native<SecKey> {
    unsafe {
        let attributes = CFDictionary::from_CFType_pairs(&[
            (CFType::wrap_under_get_rule(kSecAttrKeyType as CFTypeRef), CFType::wrap_under_get_rule(kSecAttrKeyTypeRSA as CFTypeRef)),
            (CFType::wrap_under_get_rule(kSecAttrKeyClass as CFTypeRef), CFType::wrap_under_get_rule(class as CFTypeRef)),
        ]);
        let data = CFData::from_buffer(der);
        let mut failure: CFErrorRef = std::ptr::null_mut();
        let key = SecKeyCreateWithData(data.as_concrete_TypeRef(), attributes.as_concrete_TypeRef(), &mut failure);
        if key.is_null() {
            if !failure.is_null() {
                drop(CFError::wrap_under_create_rule(failure));
            }
            Err(NativeError::Rejected)
        } else {
            Ok(SecKey::wrap_under_create_rule(key))
        }
    }
}

fn public_key(k: &RsaPublicKey) -> Native<SecKey> {
    import(&der::rsa_public(k), unsafe { kSecAttrKeyClassPublic })
}

fn private_key(k: &RsaPrivateKey) -> Native<SecKey> {
    import(&der::rsa_private(k), unsafe { kSecAttrKeyClassPrivate })
}

fn failed(context: Context, result: std::result::Result<Vec<u8>, CFError>) -> Native<Vec<u8>> {
    result.map_err(|e| NativeError::Failed(error::map(context, &e)))
}

fn oaep_algorithm(hash: Algo) -> Option<Algorithm> {
    Some(match hash {
        Algo::Sha1 => Algorithm::RSAEncryptionOAEPSHA1,
        Algo::Sha224 => Algorithm::RSAEncryptionOAEPSHA224,
        Algo::Sha256 => Algorithm::RSAEncryptionOAEPSHA256,
        Algo::Sha384 => Algorithm::RSAEncryptionOAEPSHA384,
        Algo::Sha512 => Algorithm::RSAEncryptionOAEPSHA512,
        _ => return None,
    })
}

fn pss_algorithm(hash: Algo) -> Option<Algorithm> {
    Some(match hash {
        Algo::Sha1 => Algorithm::RSASignatureDigestPSSSHA1,
        Algo::Sha224 => Algorithm::RSASignatureDigestPSSSHA224,
        Algo::Sha256 => Algorithm::RSASignatureDigestPSSSHA256,
        Algo::Sha384 => Algorithm::RSASignatureDigestPSSSHA384,
        Algo::Sha512 => Algorithm::RSASignatureDigestPSSSHA512,
        _ => return None,
    })
}

fn encryption_algorithm(padding: &RsaPadding) -> Option<Algorithm> {
    match padding {
        RsaPadding::Pkcs1 => Some(Algorithm::RSAEncryptionPKCS1),
        RsaPadding::None => Some(Algorithm::RSAEncryptionRaw),
        RsaPadding::Oaep { hash, .. } => oaep_algorithm(*hash),
    }
}

impl Primitives for Apple {
    const NAME: &'static str = "apple";

    fn oaep(&self, hash: Algo, mgf1: Algo, label: &[u8]) -> bool {
        hash == mgf1 && label.is_empty() && oaep_algorithm(hash).is_some()
    }

    fn encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>> {
        let algorithm = encryption_algorithm(padding).ok_or(NativeError::Rejected)?;
        failed(Context::Encrypt, public_key(key)?.encrypt_data(algorithm, input))
    }

    fn decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>> {
        let algorithm = encryption_algorithm(padding).ok_or(NativeError::Rejected)?;
        failed(Context::Sign, private_key(key)?.decrypt_data(algorithm, input))
    }

    fn raw_public(&self, key: &RsaPublicKey, block: &[u8]) -> Native<Vec<u8>> {
        failed(Context::Encrypt, public_key(key)?.encrypt_data(Algorithm::RSAEncryptionRaw, block))
    }

    fn raw_private(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>> {
        failed(Context::Sign, private_key(key)?.decrypt_data(Algorithm::RSAEncryptionRaw, block))
    }

    fn sign_block(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>> {
        failed(Context::Sign, private_key(key)?.create_signature(Algorithm::RSASignatureDigestPKCS1v15Raw, block))
    }

    fn verify_block(&self, key: &RsaPublicKey, block: &[u8], signature: &[u8]) -> Native<bool> {
        Ok(public_key(key)?.verify_signature(Algorithm::RSASignatureDigestPKCS1v15Raw, block, signature).unwrap_or(false))
    }

    fn pss_salt(&self, hash: Algo, mgf1: Algo, salt: PssSalt, _n: &[u8], _signing: bool) -> Option<usize> {
        let digest_salt = match salt {
            PssSalt::Digest => true,
            PssSalt::Length(len) => len as usize == hash.out_len(),
            PssSalt::MaxOrAuto => false,
        };
        (hash == mgf1 && digest_salt && pss_algorithm(hash).is_some()).then(|| hash.out_len())
    }

    fn pss_sign(&self, key: &RsaPrivateKey, hash: Algo, _salt_len: usize, hashed: &[u8]) -> Native<Vec<u8>> {
        let algorithm = pss_algorithm(hash).ok_or(NativeError::Rejected)?;
        failed(Context::Sign, private_key(key)?.create_signature(algorithm, hashed))
    }

    fn pss_verify(&self, key: &RsaPublicKey, hash: Algo, _salt_len: usize, hashed: &[u8], signature: &[u8]) -> Native<bool> {
        let algorithm = pss_algorithm(hash).ok_or(NativeError::Rejected)?;
        Ok(public_key(key)?.verify_signature(algorithm, hashed, signature).unwrap_or(false))
    }

    fn generate(&self, bits: u32, e: &[u8]) -> Option<Native<RsaPrivateKey>> {
        if trim_be(e) != PUBLIC_EXPONENT || !GENERATED_BITS.contains(&bits) {
            return None;
        }
        let mut options = GenerateKeyOptions::default();
        options.set_key_type(KeyType::rsa()).set_size_in_bits(bits).set_token(Token::Software);
        let generated = SecKey::new(&options).map_err(|e| NativeError::Failed(error::map(Context::Generate, &e)));
        Some(generated.and_then(|key| {
            let exported = key.external_representation().and_then(|data| der::parse_rsa_private(data.bytes()));
            exported.ok_or_else(|| NativeError::Failed(CryptoError::error("RSA key generation failed")))
        }))
    }
}
