//! JSON Web Key import and export for asymmetric keys (RFC 7517 / 7518 / 8037). Fields travel as
//! flat `[name, value, ...]` string lists; the JS binding builds and reads the objects.

use num_bigint_dig::BigUint;

use super::asn1;
use super::curves::EcCurve;
use super::model::{okp_public, AsymKey, EcKey, OkpKey, RsaKey, RsaPrivateParts};
use super::{KResult, SendError};

fn b64(b: &[u8]) -> String {
    lumen_common::codec::base64_encode(b, true, false)
}

fn b64_uint(n: &BigUint) -> String {
    b64(&n.to_bytes_be())
}

/// Decodes base64 or base64url, padded or not, as Node's `ByteSource::FromEncodedString` does.
fn b64_decode(s: &str) -> Vec<u8> {
    lumen_common::codec::base64_decode_lenient(s.as_bytes())
}

fn unsupported_key_type() -> SendError {
    SendError::new("TypeError", "Unsupported JWK Key Type.").with_code("ERR_CRYPTO_JWK_UNSUPPORTED_KEY_TYPE")
}

fn invalid_jwk(detail: &str) -> SendError {
    SendError::new("TypeError", format!("Invalid JWK {detail}")).with_code("ERR_CRYPTO_INVALID_JWK")
}

/// `ExportJWKInner` for an asymmetric key: the JWK members as `[name, value, ...]`. RSASSA-PSS keys
/// are exported only when `handle_rsa_pss` (WebCrypto) is set.
pub fn export(key: &AsymKey, handle_rsa_pss: bool) -> KResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut put = |k: &str, v: String| {
        out.push(k.to_string());
        out.push(v);
    };
    match key {
        AsymKey::Rsa(k) => {
            if k.pss.is_some() && !handle_rsa_pss {
                return Err(unsupported_key_type());
            }
            put("kty", "RSA".into());
            put("n", b64_uint(&k.n));
            put("e", b64_uint(&k.e));
            if let Some(p) = &k.private {
                put("d", b64_uint(&p.d));
                put("p", b64_uint(&p.p));
                put("q", b64_uint(&p.q));
                put("dp", b64_uint(&p.dp));
                put("dq", b64_uint(&p.dq));
                put("qi", b64_uint(&p.qi));
            }
        }
        AsymKey::Ec(k) => {
            let crv = k.curve.jwk_name().ok_or_else(|| {
                SendError::new("Error", format!("Unsupported JWK EC curve: {}.", k.curve.name()))
                    .with_code("ERR_CRYPTO_JWK_UNSUPPORTED_CURVE")
            })?;
            let len = k.curve.field_len();
            put("kty", "EC".into());
            put("crv", crv.into());
            put("x", b64(&k.point[1..1 + len]));
            put("y", b64(&k.point[1 + len..]));
            if let Some(d) = &k.d {
                put("d", b64(d));
            }
        }
        AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
            let crv = match key {
                AsymKey::Ed25519(_) => "Ed25519",
                AsymKey::Ed448(_) => "Ed448",
                AsymKey::X25519(_) => "X25519",
                _ => "X448",
            };
            put("crv", crv.into());
            put("x", b64(&k.public));
            if let Some(d) = &k.private {
                put("d", b64(d));
            }
            put("kty", "OKP".into());
        }
        AsymKey::Dsa(_) | AsymKey::Dh(_) => return Err(unsupported_key_type()),
    }
    Ok(out)
}

fn field<'a>(fields: &'a [String], name: &str) -> Option<&'a str> {
    fields.chunks(2).find(|c| c.len() == 2 && c[0] == name).map(|c| c[1].as_str())
}

/// `ImportJWKRsaKey` / `ImportJWKEcKey`: `fields` are the JWK's string members, `curve` the
/// requested curve name for EC keys.
pub fn import(fields: &[String], curve: Option<&str>) -> KResult<AsymKey> {
    match field(fields, "kty") {
        Some("RSA") => import_rsa(fields),
        Some("EC") => import_ec(fields, curve),
        Some(other) => Err(SendError::new("TypeError", format!("Invalid JWK data: {other} is not a supported JWK key type"))
            .with_code("ERR_CRYPTO_INVALID_JWK")),
        None => Err(SendError::new("TypeError", "Invalid JWK data").with_code("ERR_CRYPTO_INVALID_JWK")),
    }
}

fn import_rsa(fields: &[String]) -> KResult<AsymKey> {
    let bad = || invalid_jwk("RSA key");
    let uint = |name: &str| -> KResult<BigUint> {
        let s = field(fields, name).ok_or_else(bad)?;
        Ok(BigUint::from_bytes_be(&b64_decode(s)))
    };
    let n = uint("n")?;
    let e = uint("e")?;
    let private = if field(fields, "d").is_some() {
        Some(RsaPrivateParts { d: uint("d")?, p: uint("p")?, q: uint("q")?, dp: uint("dp")?, dq: uint("dq")?, qi: uint("qi")? })
    } else {
        None
    };
    Ok(AsymKey::Rsa(RsaKey { n, e, private, pss: None }))
}

fn import_ec(fields: &[String], curve: Option<&str>) -> KResult<AsymKey> {
    let bad = || invalid_jwk("EC key");
    let curve = curve.or_else(|| field(fields, "crv")).and_then(EcCurve::from_name).ok_or_else(bad)?;
    let len = curve.field_len();
    let coord = |name: &str| -> KResult<Vec<u8>> {
        let raw = b64_decode(field(fields, name).ok_or_else(bad)?);
        if raw.len() > len {
            return Err(bad());
        }
        Ok(asn1::pad_be(&raw, len))
    };
    let mut point = vec![4u8];
    point.extend(coord("x")?);
    point.extend(coord("y")?);
    let point = curve.normalize_point(&point).map_err(|_| bad())?;
    let d = match field(fields, "d") {
        Some(_) => {
            let d = coord("d")?;
            if curve.public_from_scalar(&d).map_err(|_| bad())? != point {
                return Err(bad());
            }
            Some(d)
        }
        None => None,
    };
    Ok(AsymKey::Ec(EcKey { curve, point, d, explicit: false }))
}

/// `InitEDRaw`: an OKP key from its raw public or private value. `None` when the bytes are not a
/// valid key of that type.
pub fn import_okp_raw(name: &str, data: &[u8], private: bool) -> Option<AsymKey> {
    let (kind, len, ctor): (&str, usize, fn(OkpKey) -> AsymKey) = match name {
        "Ed25519" => ("ed25519", 32, AsymKey::Ed25519),
        "Ed448" => ("ed448", 57, AsymKey::Ed448),
        "X25519" => ("x25519", 32, AsymKey::X25519),
        "X448" => ("x448", 56, AsymKey::X448),
        _ => return None,
    };
    if data.len() != len {
        return None;
    }
    if private {
        let public = okp_public(kind, data).ok()?;
        Some(ctor(OkpKey { public, private: Some(data.to_vec()) }))
    } else {
        Some(ctor(OkpKey { public: data.to_vec(), private: None }))
    }
}
