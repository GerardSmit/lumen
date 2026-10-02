//! Signatures (RSA PKCS#1 v1.5 and PSS, DSA, ECDSA, Ed25519, Ed448) and RSA encryption padding. RSA
//! and DSA go through `lumen_crypto::backend()`; this module resolves Node's options (digest names,
//! PSS key restrictions, salt lengths, DER vs IEEE P1363 encoding) into the backend's neutral
//! schemes. ECDSA and EdDSA run on RustCrypto.

use lumen::embed::OpError;
use lumen_crypto::{backend, CryptoError, PssSalt, RsaPadding, RsaScheme};
use signature::hazmat::{PrehashSigner, PrehashVerifier, RandomizedPrehashSigner};
use signature::Signer;

use super::keys::{asn1, with_ec_curve, AsymKey, DsaKey, EcCurve, EcKey, PssParams, RsaKey};
use super::op_error;
use crate::hash::{self, Algo};


#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
use super::*;

const RSA_PKCS1_PADDING: i32 = 1;
const RSA_NO_PADDING: i32 = 3;
const RSA_PKCS1_OAEP_PADDING: i32 = 4;
const RSA_PKCS1_PSS_PADDING: i32 = 6;
const SALT_DIGEST: i32 = -1;
const SALT_MAX_OR_AUTO: i32 = -2;

fn ossl(hex: &str, library: &str, reason: &str) -> OpError {
    op_error(CryptoError::openssl(hex, library, reason))
}

fn illegal_padding() -> OpError {
    ossl("1C8000A5", "Provider routines", "illegal or unsupported padding mode")
}

fn invalid_digest() -> OpError {
    ossl("1C80007A", "Provider routines", "invalid digest")
}

fn digest_not_allowed() -> OpError {
    ossl("1C8000AE", "Provider routines", "digest not allowed")
}

fn invalid_salt() -> OpError {
    ossl("1C8000A8", "Provider routines", "invalid salt length")
}

fn unsupported_keytype() -> OpError {
    ossl("03000096", "digital envelope routines", "operation not supported for this keytype")
}

fn sign_failed() -> OpError {
    OpError::error("Failed to sign").with_code("ERR_CRYPTO_OPERATION_FAILED")
}

fn digest_algo(name: &str) -> Result<Algo, OpError> {
    Algo::from_name(name)
        .ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))
}

struct PssConfig {
    hash: Algo,
    mgf1: Algo,
    salt: i32,
}

/// The digests and salt length of a PSS operation: the key's restrictions when it has any (the
/// requested digest must match, the salt may not be shorter), otherwise the request.
fn pss_config(k: &RsaKey, hash: Option<&str>, salt: Option<i32>) -> Result<PssConfig, OpError> {
    let requested = hash.map(digest_algo).transpose()?;
    let restriction: Option<PssParams> = k.pss.flatten();
    if let Some(r) = restriction {
        let key_hash = digest_algo(r.hash)?;
        if requested.is_some_and(|h| h != key_hash) {
            return Err(digest_not_allowed());
        }
        let salt = match salt {
            Some(s) if s >= 0 && (s as u32) < r.salt_length => return Err(invalid_salt()),
            Some(s) => s,
            None => r.salt_length as i32,
        };
        return Ok(PssConfig { hash: key_hash, mgf1: digest_algo(r.mgf1_hash)?, salt });
    }
    let hash = requested.unwrap_or(Algo::Sha256);
    let salt = salt.unwrap_or(SALT_MAX_OR_AUTO);
    Ok(PssConfig { hash, mgf1: hash, salt })
}

/// `None` for a salt length no scheme can mean.
fn pss_salt(salt: i32) -> Option<PssSalt> {
    match salt {
        SALT_MAX_OR_AUTO => Some(PssSalt::MaxOrAuto),
        SALT_DIGEST => Some(PssSalt::Digest),
        s if s >= 0 => Some(PssSalt::Length(s as u32)),
        _ => None,
    }
}

fn rsa_default_padding(k: &RsaKey) -> i32 {
    if k.pss.is_some() {
        RSA_PKCS1_PSS_PADDING
    } else {
        RSA_PKCS1_PADDING
    }
}

fn default_hash(hash: Option<&str>) -> Result<Algo, OpError> {
    hash.map(digest_algo).transpose().map(|h| h.unwrap_or(Algo::Sha256))
}

/// The scheme and the digest to hash the message with. `verify` makes an impossible salt length
/// a failed verification instead of an error (the caller handles `None`).
fn rsa_scheme(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, verify: bool) -> Result<Option<(RsaScheme, Algo)>, OpError> {
    let padding = padding.unwrap_or_else(|| rsa_default_padding(k));
    match padding {
        RSA_PKCS1_PADDING if k.pss.is_none() => {
            let algo = default_hash(hash)?;
            Ok(Some((RsaScheme::Pkcs1 { hash: algo }, algo)))
        }
        RSA_PKCS1_PSS_PADDING => {
            let cfg = pss_config(k, hash, salt)?;
            match pss_salt(cfg.salt) {
                Some(salt) => Ok(Some((RsaScheme::Pss { hash: cfg.hash, mgf1: cfg.mgf1, salt }, cfg.hash))),
                None if verify => Ok(None),
                None => Err(invalid_salt()),
            }
        }
        _ => Err(illegal_padding()),
    }
}

fn rsa_sign(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let (scheme, algo) = rsa_scheme(k, hash, padding, salt, false)?.expect("signing schemes are never absent");
    let key = k.private_parts().map_err(OpError::from)?;
    backend().rsa_sign(&key, &scheme, &hash::digest(algo, data)).map_err(op_error)
}

fn rsa_verify(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let Some((scheme, algo)) = rsa_scheme(k, hash, padding, salt, true)? else { return Ok(false) };
    backend().rsa_verify(&k.public_parts(), &scheme, &hash::digest(algo, data), sig).map_err(op_error)
}

fn der_to_rs(der: &[u8], len: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut outer = asn1::Reader::new(der);
    let mut seq = outer.sequence()?;
    outer.finish()?;
    let r = seq.uint()?;
    let s = seq.uint()?;
    seq.finish()?;
    if r.len() > len || s.len() > len {
        return None;
    }
    Some((asn1::pad_be(r, len), asn1::pad_be(s, len)))
}

fn rs_to_der(r: &[u8], s: &[u8]) -> Vec<u8> {
    asn1::seq(&[&asn1::uint(r), &asn1::uint(s)])
}

fn ec_prehash(hash: &[u8], field_len: usize) -> Vec<u8> {
    if hash.len() >= field_len {
        return hash.to_vec();
    }
    let mut out = vec![0u8; field_len - hash.len()];
    out.extend_from_slice(hash);
    out
}

fn ecdsa_sign(k: &EcKey, hashed: &[u8], p1363: bool) -> Result<Vec<u8>, OpError> {
    let d = k.d.as_ref().ok_or_else(sign_failed)?;
    let flen = k.curve.field_len();
    let prehash = ec_prehash(hashed, flen);
    let scalar = asn1::pad_be(d, flen);
    macro_rules! sign_with {
        ($C:ty) => {{
            let sk = ecdsa::SigningKey::<$C>::from_slice(&scalar).map_err(|_| sign_failed())?;
            let sig: ecdsa::Signature<$C> = sk.sign_prehash(&prehash).map_err(|_| sign_failed())?;
            sig.to_bytes().to_vec()
        }};
    }
    let rs = match k.curve {
        EcCurve::P256 => sign_with!(p256::NistP256),
        EcCurve::P384 => sign_with!(p384::NistP384),
        EcCurve::Secp256k1 => sign_with!(k256::Secp256k1),
        EcCurve::P521 => {
            let sk = p521::ecdsa::SigningKey::from_slice(&scalar).map_err(|_| sign_failed())?;
            let sig = sk.sign_prehash_with_rng(&mut lumen_crypto::SysRng, &prehash).map_err(|_| sign_failed())?;
            sig.to_bytes().to_vec()
        }
    };
    if p1363 {
        Ok(rs)
    } else {
        Ok(rs_to_der(&rs[..flen], &rs[flen..]))
    }
}

fn ecdsa_verify(k: &EcKey, hashed: &[u8], sig: &[u8], p1363: bool) -> bool {
    let flen = k.curve.field_len();
    let rs = if p1363 {
        if sig.len() != 2 * flen {
            return false;
        }
        sig.to_vec()
    } else {
        match der_to_rs(sig, flen) {
            Some((r, s)) => [r, s].concat(),
            None => return false,
        }
    };
    let prehash = ec_prehash(hashed, flen);
    with_ec_curve!(k.curve, C => {
        let Ok(vk) = ecdsa::VerifyingKey::<C>::from_sec1_bytes(&k.point) else { return false };
        let Ok(sig) = ecdsa::Signature::<C>::from_slice(&rs) else { return false };
        let sig = sig.normalize_s().unwrap_or(sig);
        vk.verify_prehash(&prehash, &sig).is_ok()
    })
}

fn dsa_sign(k: &DsaKey, hashed: &[u8], p1363: bool) -> Result<Vec<u8>, OpError> {
    let der = backend().dsa_sign(&k.private_parts().map_err(OpError::from)?, hashed).map_err(op_error)?;
    if !p1363 {
        return Ok(der);
    }
    let len = k.q.bits().div_ceil(8);
    let (r, s) = der_to_rs(&der, len).ok_or_else(sign_failed)?;
    Ok([r, s].concat())
}

fn dsa_verify(k: &DsaKey, hashed: &[u8], sig: &[u8], p1363: bool) -> bool {
    let der = if p1363 {
        let len = k.q.bits().div_ceil(8);
        if sig.len() != 2 * len {
            return false;
        }
        rs_to_der(&sig[..len], &sig[len..])
    } else {
        sig.to_vec()
    };
    backend().dsa_verify(&k.public_parts(), hashed, &der).unwrap_or(false)
}

fn sign_with(key: &AsymKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8]) -> Result<Vec<u8>, OpError> {
    match key {
        AsymKey::Rsa(k) => rsa_sign(k, hash, padding, salt, data),
        AsymKey::Ec(k) => {
            let algo = default_hash(hash)?;
            ecdsa_sign(k, &hash::digest(algo, data), p1363)
        }
        AsymKey::Dsa(k) => {
            let algo = default_hash(hash)?;
            dsa_sign(k, &hash::digest(algo, data), p1363)
        }
        AsymKey::Ed25519(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            Ok(k.ed25519_signing()?.sign(data).to_bytes().to_vec())
        }
        AsymKey::Ed448(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let sig = k.ed448_signing()?.try_sign(data).map_err(|_| sign_failed())?;
            Ok(sig.to_bytes().to_vec())
        }
        _ => Err(unsupported_keytype()),
    }
}

fn verify_with(key: &AsymKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    match key {
        AsymKey::Rsa(k) => rsa_verify(k, hash, padding, salt, data, sig),
        AsymKey::Ec(k) => {
            let algo = default_hash(hash)?;
            Ok(ecdsa_verify(k, &hash::digest(algo, data), sig, p1363))
        }
        AsymKey::Dsa(k) => {
            let algo = default_hash(hash)?;
            Ok(dsa_verify(k, &hash::digest(algo, data), sig, p1363))
        }
        AsymKey::Ed25519(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else { return Ok(false) };
            Ok(k.ed25519_verifying()?.verify_strict(data, &sig).is_ok())
        }
        AsymKey::Ed448(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let Ok(sig) = ed448_goldilocks_plus::Signature::try_from(sig) else { return Ok(false) };
            Ok(k.ed448_verifying()?.verify_raw(&sig, data).is_ok())
        }
        _ => Err(unsupported_keytype()),
    }
}

/// `sign` of `Sign` / `crypto.sign`: the signature of `data` (DER, or `r || s` for `p1363`).
#[op(name = "sigSign")]
fn sig_sign(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    sign_with(&key, hash.as_deref(), padding, salt, p1363, data)
}

#[op(name = "sigVerify")]
fn sig_verify(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    verify_with(&key, hash.as_deref(), padding, salt, p1363, data, sig)
}

fn oaep_hash(name: Option<&str>) -> Result<Algo, OpError> {
    match name {
        None => Ok(Algo::Sha1),
        Some(n) => Algo::from_name(n)
            .filter(|a| !a.is_xof() && *a != Algo::Md5Sha1)
            .ok_or_else(|| OpError::error("Digest method not supported").with_code("ERR_OSSL_EVP_UNSUPPORTED")),
    }
}

/// `publicEncrypt` (0), `privateDecrypt` (1), `privateEncrypt` (2) and `publicDecrypt` (3).
#[op(name = "rsaCipher")]
fn rsa_cipher(
    operation: u32,
    kind: u32,
    der: &[u8],
    input: &[u8],
    padding: i32,
    oaep: Option<String>,
    label: Option<Vec<u8>>,
) -> Result<Vec<u8>, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    let AsymKey::Rsa(k) = &key else { return Err(unsupported_keytype()) };
    let private_key_op = matches!(operation, 1 | 2);
    if private_key_op && k.private.is_none() {
        return Err(OpError::error("key is not a private key"));
    }
    let padding = match padding {
        RSA_PKCS1_OAEP_PADDING => {
            let hash = oaep_hash(oaep.as_deref())?;
            RsaPadding::Oaep { hash, mgf1: hash, label: label.unwrap_or_default() }
        }
        RSA_PKCS1_PADDING => RsaPadding::Pkcs1,
        RSA_NO_PADDING => RsaPadding::None,
        _ => return Err(illegal_padding()),
    };
    let backend = backend();
    let out = match operation {
        0 => backend.rsa_encrypt(&k.public_parts(), &padding, input),
        1 => backend.rsa_decrypt(&k.private_parts().map_err(OpError::from)?, &padding, input),
        2 => backend.rsa_private_encrypt(&k.private_parts().map_err(OpError::from)?, &padding, input),
        _ => backend.rsa_public_decrypt(&k.public_parts(), &padding, input),
    };
    out.map_err(op_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // The 1024-bit key and the vectors were produced by `openssl` 3 (genpkey, dgst -sign, pkeyutl
    // -encrypt / -sign) over MSG and PLAIN; `lumen-crypto` checks them per backend, these exercise
    // the option handling on top.
    const KEY: &str = "MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBAJtNDLz0UW46tFwXTJtyHEdrOhK2sf9U50jA1RmUmXL78rYSfjOHX7aTE6bAs+5nli6qXC2UhJokBGN6zHviQu/INLejVSD7Tp6nPMrvtkh82vl5IY6sLX3F5TqY7Rp7Bjf+yysg8drMe5LbjqMAJjdM5QtzlKIZNyDk8EUkWF/rAgMBAAECgYBoIfnwmUIgz2wwc88CTDl6CgQemDIyKxQKTIKXbHSYDShpvWyx0Iv1OBltLrl3mi2xjLnSNkvTr2Lh8W07hDOs2PN/hYX1/eRmnld1NPGCbJFG4eYnSiDwNz7jet5JUtm0IrwRzkQHmqJMxDIqg/OBY8aIfj70Ytz8BnVEiXLQgQJBAMvUhNGd1teAX64/H4Boelwmw70FXvjVLLDj1E9NqII7fhOJGosD1ksSu1CFgBdiGa7dw/ylxzKntIfgPq/ALSkCQQDDDMXxy3LxdWQZw+fbA9jbqST8NXxdH/U5FTDSWdcBHdQVwqrhZT64/3jZ2b8vnepPRE55XBTR3Bg4y+sPL7LzAkB42R+GSFbAnlQcM0CyGT+ysykKQMz2Ky28EtglzJ1D2ZH+cyNRmIzNJeX4763qLzea/dDdUkywM85NYR7JhN9BAkAvORl3mBVFJnHM1yR8XysSy5nbwitQ9JrPbjT6yKuIZqthdVcf6P5NlfSxccmbArWm6VfChCu6P3pRzfUkIR1HAkBuuZDNn0dfPCVg6JlOY4DRrSnz2rjq0QicshjzacDdGJQsroOz7pyKBXq4mnZTkp3GbiZCNOOA25aV2jMTMvj5";
    const MSG: &[u8] = b"lumen rsa vectors";
    const PLAIN: &[u8] = b"oaep secret";
    const PKCS1_SHA256: &str = "4684dcaf60d92662eba75673a2b818fd35da7aa04350add326c0ed80967fc8bdbeae98fb62411e8adaffd30f280f23cbc05b1d77ca68ffcf7246a16175eb156b24fa31af7a524aa9063528c3e9be026a41e36840d3dfa2b131d4162f9d8347615c5164a88badb8915585534d46d5b5f48e38dc2b2151bf739a98a5b00bd3ccf5";
    const PSS_MGF1_SHA1: &str = "0c0323a4283101cd2d59917922e26234accd56f475bf20248017eee43376e82877cc288486a1c0cdbd864f6e89d1602fceb4b189dd283a25acdc6b6ad1ebc9e6d8e102d8ded6a950a513885eb2d4acdbea61f4887ed305b97c7c1e0efabde1164f8c6d830909ecce77622992bd2971d9454aab0dc6cd66ee750b145a199d218d";
    const OAEP_SHA256_LABEL: &str = "6ec8febf285d7c1e9caaa605ad27adae951ce69d5cbd892facf6cb3d7ba953c28ca0c355dd462f4fcd8b35a477ad8cfe84f7353c9ed04fb609c00b68418cce206de3f84f66bb4bc401f321b6d0ff764893030f6f67d492ca5fb284de022b7cdfc541bda0bb642b28284d08618054fbdcdcc7adadb6a381da402a4fa2c0e075d5";
    const PRIVATE_ENCRYPTED: &str = "8402076ded2788f9645b875445f0d7d696a540e20b69834f7b408a7e38867adc713dfcae5e82648ffd1451c483e8026f80127fd9b0a78321f3ede7e791919018a1b37e021be5c8a2e7d7ec9b90bf978f91fe8632fef63bf0d5399f4d101df2aeca5b01fb46e70db13e87d9c0f3f2a51383df2ad8be224449673df8667391f81b";

    fn pkcs8() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(KEY).unwrap()
    }

    fn key() -> RsaKey {
        match AsymKey::from_pkcs8_der(&pkcs8()).unwrap() {
            AsymKey::Rsa(k) => k,
            _ => unreachable!(),
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn pkcs1_v15_signature_matches_openssl() {
        let k = key();
        assert_eq!(rsa_sign(&k, Some("sha256"), None, None, MSG).unwrap(), hex(PKCS1_SHA256));
        assert!(rsa_verify(&k, Some("sha256"), None, None, MSG, &hex(PKCS1_SHA256)).unwrap());
        assert!(!rsa_verify(&k, Some("sha256"), None, None, b"other", &hex(PKCS1_SHA256)).unwrap());
        assert!(!rsa_verify(&k, Some("sha512"), None, None, MSG, &hex(PKCS1_SHA256)).unwrap());
    }

    #[test]
    fn pss_key_restrictions_decide_the_scheme() {
        let mut k = key();
        k.pss = Some(Some(PssParams { hash: "sha256", mgf1_hash: "sha1", salt_length: 0 }));
        assert!(rsa_verify(&k, None, None, Some(SALT_MAX_OR_AUTO), MSG, &hex(PSS_MGF1_SHA1)).unwrap());
        let sig = rsa_sign(&k, None, None, Some(16), MSG).unwrap();
        assert!(rsa_verify(&k, None, None, Some(16), MSG, &sig).unwrap());
        assert!(!rsa_verify(&k, None, None, Some(15), MSG, &sig).unwrap());
        let wrong_digest = rsa_sign(&k, Some("sha512"), None, None, MSG).unwrap_err().to_string();
        assert!(wrong_digest.contains("digest not allowed"), "{wrong_digest}");
        let short_salt = rsa_sign(&k, None, None, Some(-3), MSG).unwrap_err().to_string();
        assert!(short_salt.contains("illegal") || short_salt.contains("salt"), "{short_salt}");
    }

    #[test]
    fn rsa_cipher_runs_every_operation() {
        let der = pkcs8();
        let public = AsymKey::Rsa(key()).to_spki_der();
        let label = vec![0x00, 0xff, 0x10];
        let oaep = rsa_cipher(1, 2, &der, &hex(OAEP_SHA256_LABEL), RSA_PKCS1_OAEP_PADDING, Some("sha256".into()), Some(label.clone())).unwrap();
        assert_eq!(oaep, PLAIN);
        let ct = rsa_cipher(0, 1, &public, PLAIN, RSA_PKCS1_OAEP_PADDING, None, None).unwrap();
        assert_eq!(rsa_cipher(1, 2, &der, &ct, RSA_PKCS1_OAEP_PADDING, None, None).unwrap(), PLAIN);
        assert_eq!(rsa_cipher(2, 2, &der, PLAIN, RSA_PKCS1_PADDING, None, None).unwrap(), hex(PRIVATE_ENCRYPTED));
        assert_eq!(rsa_cipher(3, 1, &public, &hex(PRIVATE_ENCRYPTED), RSA_PKCS1_PADDING, None, None).unwrap(), PLAIN);
        let unsupported = rsa_cipher(0, 1, &public, PLAIN, 99, None, None).unwrap_err().to_string();
        assert!(unsupported.contains("illegal or unsupported padding mode"), "{unsupported}");
        let public_only = rsa_cipher(1, 1, &public, PLAIN, RSA_PKCS1_PADDING, None, None).unwrap_err().to_string();
        assert!(public_only.contains("not a private key"), "{public_only}");
    }
}
}
