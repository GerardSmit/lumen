//! Asymmetric keys: the Rust key model behind `KeyObjectHandle` (binding/20_keys.js), key import /
//! export in every Node format, JWK, and key pair generation.
//!
//! A JS `KeyObjectHandle` stores a public key as SPKI DER and a private key as PKCS#8 DER (RSA-PSS
//! keys keep their `RSASSA-PSS` algorithm identifier and parameters). Native code that needs a key
//! parses those bytes with [`AsymKey::from_handle`] (or [`AsymKey::from_spki_der`] /
//! [`AsymKey::from_pkcs8_der`]) and then takes the crate key it works with:
//!
//! * RSA / RSA-PSS: [`RsaKey::rsa_public`], [`RsaKey::rsa_private`] (`rsa` crate);
//!   [`RsaKey::pss`] carries the PSS restrictions, [`AsymKey::type_name`] is `rsa` or `rsa-pss`.
//! * DSA: [`DsaKey::dsa_verifying`], [`DsaKey::dsa_signing`] (`dsa` crate).
//! * EC: [`EcKey::curve`] ([`EcCurve`]) plus [`EcKey::public_key`] / [`EcKey::secret_key`] generic
//!   over the curve type; [`with_ec_curve!`] binds the curve type of an [`EcCurve`] value.
//! * Ed25519 / Ed448 / X25519 / X448: [`OkpKey`] raw bytes and its `ed25519_*`, `ed448_*`,
//!   `x25519_*` accessors; [`x448_mul`] is X448(k, u).
//! * DH: [`DhKey`] (`p`, `g`, optional `q`, `y`, `x` as `num-bigint-dig` integers); [`dh_group`]
//!   holds the MODP groups.
//!
//! Errors are [`SendError`]s carrying Node's codes; OpenSSL-style messages
//! (`error:<hex>:<library>::<reason>`) get their `library`/`reason` fields in the JS binding.

use lumen::embed::OpError;
pub use lumen::embed::SendError;

pub(crate) mod asn1;
mod curves;
mod dh_groups;
mod gen;
mod jwk;
mod model;
mod pem;

pub use curves::{with_ec_curve, EcCurve};
pub use dh_groups::dh_group;
pub use gen::{safe_prime, safe_prime_congruent};
pub use model::{pss_hash, x448_mul, AsymKey, DsaKey, EcKey, PssParams, RsaKey};
pub use pem::KeyCipher;

pub(crate) use bindings::*;

#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
    use super::*;

    /// Result of key operations.
    pub type KResult<T> = Result<T, SendError>;

    fn ossl(hex: &str, library: &str, reason: &str, code: &'static str) -> SendError {
        SendError::new("Error", format!("error:{hex}:{library}::{reason}")).with_code(code)
    }

    /// OpenSSL 3's error for undecodable key input.
    pub fn decoder_unsupported() -> SendError {
        ossl(
            "1E08010C",
            "DECODER routines",
            "unsupported",
            "ERR_OSSL_UNSUPPORTED",
        )
    }

    pub(crate) fn invalid_private() -> SendError {
        decoder_unsupported()
    }

    pub(crate) fn invalid_point() -> SendError {
        decoder_unsupported()
    }

    /// An encrypted PEM key read without a passphrase (OpenSSL 3's password callback refusal).
    pub fn interrupted() -> SendError {
        ossl(
            "07880109",
            "common libcrypto routines",
            "interrupted or cancelled",
            "ERR_OSSL_CRYPTO_INTERRUPTED_OR_CANCELLED",
        )
    }

    /// An encrypted DER PKCS#8 key read without a passphrase.
    pub fn missing_passphrase() -> SendError {
        SendError::new("TypeError", "Passphrase required for encrypted key")
            .with_code("ERR_MISSING_PASSPHRASE")
    }

    pub fn bad_decrypt() -> SendError {
        ossl(
            "1C800064",
            "Provider routines",
            "bad decrypt",
            "ERR_OSSL_BAD_DECRYPT",
        )
    }

    pub(crate) fn unknown_cipher() -> SendError {
        SendError::new("Error", "Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER")
    }

    fn op_err(e: SendError) -> OpError {
        e.into()
    }

    // ---- ops -----------------------------------------------------------------------------------

    /// `KeyObjectHandle.init` for asymmetric input: `kind` 1 parses a public key (from public or
    /// private input), 2 a private key. Returns the SPKI / PKCS#8 DER the handle stores.
    #[op(name = "keyImport")]
    fn key_import(
        kind: u32,
        data: &[u8],
        format: u32,
        enc: Option<u32>,
        passphrase: Option<&[u8]>,
    ) -> Result<Vec<u8>, OpError> {
        if kind == 2 {
            let k = pem::import_private(data, format, enc, passphrase).map_err(op_err)?;
            k.to_pkcs8_der().map_err(op_err)
        } else {
            Ok(pem::import_public(data, format, enc, passphrase)
                .map_err(op_err)?
                .to_spki_der())
        }
    }

    /// The SPKI of the public half of a PKCS#8 private key.
    #[op(name = "keyToPublic")]
    fn key_to_public(pkcs8: &[u8]) -> Result<Vec<u8>, OpError> {
        Ok(AsymKey::from_pkcs8_der(pkcs8)
            .map_err(op_err)?
            .to_spki_der())
    }

    /// `asymmetricKeyType` of a handle's key.
    #[op(name = "keyType")]
    fn key_type(kind: u32, der: &[u8]) -> Result<&'static str, OpError> {
        Ok(AsymKey::from_handle(kind, der).map_err(op_err)?.type_name())
    }

    /// `keyDetail()` as `[name, value, ...]`: numbers in decimal, `publicExponent` in hex.
    #[op(name = "keyDetail")]
    fn key_detail(kind: u32, der: &[u8]) -> Result<Vec<String>, OpError> {
        let key = AsymKey::from_handle(kind, der).map_err(op_err)?;
        let mut out = Vec::new();
        let mut put = |k: &str, v: String| {
            out.push(k.to_string());
            out.push(v);
        };
        match &key {
            AsymKey::Rsa(k) => {
                put("modulusLength", k.modulus_bits().to_string());
                put("publicExponent", k.e.to_str_radix(16));
                if let Some(Some(p)) = &k.pss {
                    put("hashAlgorithm", p.hash.to_string());
                    put("mgf1HashAlgorithm", p.mgf1_hash.to_string());
                    put("saltLength", p.salt_length.to_string());
                }
            }
            AsymKey::Dsa(k) => {
                put("modulusLength", k.p.bits().to_string());
                put("divisorLength", k.q.bits().to_string());
            }
            AsymKey::Ec(k) => put("namedCurve", k.curve.name().to_string()),
            _ => {}
        }
        Ok(out)
    }

    /// `KeyObjectHandle.export(format, type, cipher, passphrase)` for asymmetric keys: PEM text (as
    /// bytes) or DER.
    #[op(name = "keyExport")]
    fn key_export(
        kind: u32,
        der: &[u8],
        format: u32,
        enc: u32,
        cipher: Option<String>,
        passphrase: Option<&[u8]>,
    ) -> Result<Vec<u8>, OpError> {
        let key = AsymKey::from_handle(kind, der).map_err(op_err)?;
        let out = if kind == 2 {
            pem::export_private(&key, format, enc, cipher.as_deref(), passphrase)
        } else {
            pem::export_public(&key, format, enc)
        };
        Ok(out.map_err(op_err)?.into_bytes())
    }

    /// `exportJwk` for asymmetric keys: the members as `[name, value, ...]`.
    #[op(name = "keyExportJwk")]
    fn key_export_jwk(kind: u32, der: &[u8], handle_rsa_pss: bool) -> Result<Vec<String>, OpError> {
        let key = AsymKey::from_handle(kind, der).map_err(op_err)?;
        jwk::export(&key, handle_rsa_pss).map_err(op_err)
    }

    /// `initJwk` for RSA / EC JWKs given as `[name, value, ...]`: `[kind, der]`.
    #[op(name = "keyImportJwk")]
    fn key_import_jwk(
        fields: Vec<String>,
        curve: Option<String>,
    ) -> Result<(u32, Vec<u8>), OpError> {
        let key = jwk::import(&fields, curve.as_deref()).map_err(op_err)?;
        Ok(if key.is_private() {
            (2, key.to_handle_der())
        } else {
            (1, key.to_spki_der())
        })
    }

    /// `initEDRaw`: the handle DER of a raw OKP key, or `null` when the bytes are not a valid key.
    #[op(name = "keyImportOkpRaw")]
    fn key_import_okp_raw(name: &str, data: &[u8], private: bool) -> Option<Vec<u8>> {
        jwk::import_okp_raw(name, data, private).map(|k| k.to_handle_der())
    }

    /// `initECRaw`: the SPKI of a raw (SEC1) EC public point, or `null` when it is not on the curve.
    #[op(name = "keyImportEcRaw")]
    fn key_import_ec_raw(curve: &str, data: &[u8]) -> Option<Vec<u8>> {
        let curve = EcCurve::from_name(curve)?;
        let point = curve.normalize_point(data).ok()?;
        Some(
            AsymKey::Ec(EcKey {
                curve,
                point,
                d: None,
                explicit: false,
            })
            .to_spki_der(),
        )
    }

    /// `equals` for two asymmetric handles of the same kind.
    #[op(name = "keyEquals")]
    fn key_equals(kind: u32, a: &[u8], b: &[u8]) -> bool {
        match (AsymKey::from_handle(kind, a), AsymKey::from_handle(kind, b)) {
            (Ok(a), Ok(b)) => a.public_eq(&b),
            _ => false,
        }
    }

    /// `checkEcKeyData`: whether an EC key's point (and scalar) are valid.
    #[op(name = "keyCheckEc")]
    fn key_check_ec(kind: u32, der: &[u8]) -> bool {
        match AsymKey::from_handle(kind, der) {
            Ok(AsymKey::Ec(k)) => match &k.d {
                Some(d) => k.curve.public_from_scalar(d).is_ok_and(|p| p == k.point),
                None => k.curve.normalize_point(&k.point).is_ok(),
            },
            _ => false,
        }
    }

    /// WebCrypto raw export of a public key: the uncompressed EC point or the raw OKP key.
    #[op(name = "keyRawPublic")]
    fn key_raw_public(spki: &[u8]) -> Result<Vec<u8>, OpError> {
        match AsymKey::from_spki_der(spki).map_err(op_err)? {
            AsymKey::Ec(k) => Ok(k.point),
            AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
                Ok(k.public)
            }
            _ => Err(OpError::error("Unsupported key type")),
        }
    }

    /// Whether `name` is a cipher usable for private key encryption.
    #[op(name = "keyCipherKnown")]
    fn key_cipher_known(name: &str) -> bool {
        KeyCipher::from_name(name).is_some()
    }

    /// `getCurves()`: the supported curves, sorted.
    #[op(name = "keyCurves")]
    fn key_curves() -> Vec<String> {
        let mut v: Vec<String> = curves::ALL.iter().map(|c| c.name().to_string()).collect();
        v.sort();
        v
    }

    /// Whether `name` names a supported EC curve.
    #[op(name = "keyCurveKnown")]
    fn key_curve_known(name: &str) -> bool {
        EcCurve::from_name(name).is_some()
    }

    /// Whether `name` is a known MODP group.
    #[op(name = "keyDhGroupKnown")]
    fn key_dh_group_known(name: &str) -> bool {
        dh_group(name).is_some()
    }

    type KeyPair = (Vec<u8>, Vec<u8>);

    fn pair(k: AsymKey) -> KeyPair {
        (k.to_spki_der(), k.to_handle_der())
    }

    fn pss_config(
        pss: bool,
        hash: Option<&str>,
        mgf1: Option<&str>,
        salt: Option<i32>,
    ) -> Option<Option<PssParams>> {
        if !pss {
            return None;
        }
        let md = hash.and_then(pss_hash);
        let mgf1 = mgf1.and_then(pss_hash).or(md);
        if md.is_none() && mgf1.is_none() && salt.is_none() {
            return Some(None);
        }
        let salt_length = match salt {
            Some(s) => s.max(0) as u32,
            None => md.map_or(20, |m| m.1),
        };
        Some(Some(PssParams {
            hash: md.map_or("sha1", |m| m.0),
            mgf1_hash: mgf1.map_or("sha1", |m| m.0),
            salt_length,
        }))
    }

    /// RSA / RSA-PSS key generation: `[spki, pkcs8]`.
    #[op(name = "keygenRsa")]
    fn keygen_rsa(
        bits: u32,
        e: u32,
        pss: bool,
        hash: Option<String>,
        mgf1: Option<String>,
        salt: Option<i32>,
    ) -> Result<KeyPair, OpError> {
        let cfg = pss_config(pss, hash.as_deref(), mgf1.as_deref(), salt);
        gen::rsa(bits, e, cfg).map(pair).map_err(op_err)
    }

    #[op(async, name = "keygenRsaAsync")]
    fn keygen_rsa_async(
        bits: u32,
        e: u32,
        pss: bool,
        hash: Option<String>,
        mgf1: Option<String>,
        salt: Option<i32>,
    ) -> Result<KeyPair, SendError> {
        let cfg = pss_config(pss, hash.as_deref(), mgf1.as_deref(), salt);
        gen::rsa(bits, e, cfg).map(pair)
    }

    /// DSA key generation (`divisor` < 0 picks OpenSSL's default): `[spki, pkcs8]`.
    #[op(name = "keygenDsa")]
    fn keygen_dsa(bits: u32, divisor: i32) -> Result<KeyPair, OpError> {
        gen::dsa(bits, u32::try_from(divisor).ok())
            .map(pair)
            .map_err(op_err)
    }

    #[op(async, name = "keygenDsaAsync")]
    fn keygen_dsa_async(bits: u32, divisor: i32) -> Result<KeyPair, SendError> {
        gen::dsa(bits, u32::try_from(divisor).ok()).map(pair)
    }

    /// EC key generation on a named curve (`explicit` writes explicit parameters): `[spki, pkcs8]`.
    #[op(name = "keygenEc")]
    fn keygen_ec(curve: &str, explicit: bool) -> Result<KeyPair, OpError> {
        let c = EcCurve::from_name(curve).ok_or_else(|| {
            OpError::type_error("Invalid EC curve name").with_code("ERR_CRYPTO_INVALID_CURVE")
        })?;
        Ok(pair(gen::ec(c, explicit)))
    }

    /// Ed25519 / Ed448 / X25519 / X448 key generation (`kind` as `asymmetricKeyType`).
    #[op(name = "keygenOkp")]
    fn keygen_okp(kind: &str) -> Result<KeyPair, OpError> {
        gen::okp(kind).map(pair).map_err(op_err)
    }

    fn dh_keygen(
        group: Option<String>,
        prime: Option<Vec<u8>>,
        prime_len: Option<u32>,
        g: u32,
    ) -> KResult<KeyPair> {
        let (p, g) = match (group, prime, prime_len) {
            (Some(name), _, _) => dh_group(&name).ok_or_else(|| {
                SendError::new("Error", "Unknown DH group").with_code("ERR_CRYPTO_UNKNOWN_DH_GROUP")
            })?,
            (None, Some(p), _) => (
                num_bigint_dig::BigUint::from_bytes_be(&p),
                num_bigint_dig::BigUint::from(g),
            ),
            (None, None, Some(bits)) => (gen::safe_prime(bits)?, num_bigint_dig::BigUint::from(g)),
            _ => return Err(SendError::new("Error", "Invalid DH parameters")),
        };
        gen::dh(p, g).map(pair)
    }

    /// DH key generation from a MODP group name, a prime, or a prime length: `[spki, pkcs8]`.
    #[op(name = "keygenDh")]
    fn keygen_dh(
        group: Option<String>,
        prime: Option<Vec<u8>>,
        prime_len: Option<u32>,
        g: u32,
    ) -> Result<KeyPair, OpError> {
        dh_keygen(group, prime, prime_len, g).map_err(op_err)
    }

    #[op(async, name = "keygenDhAsync")]
    fn keygen_dh_async(
        group: Option<String>,
        prime: Option<Vec<u8>>,
        prime_len: Option<u32>,
        g: u32,
    ) -> Result<KeyPair, SendError> {
        dh_keygen(group, prime, prime_len, g)
    }
}
