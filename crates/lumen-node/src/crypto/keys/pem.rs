//! Key import from user input (PEM / DER in every encoding Node accepts) and export to PEM / DER,
//! including passphrase protection: PKCS#8 PBES2 (via `pkcs8` / `pkcs5`) and the legacy OpenSSL
//! `Proc-Type: 4,ENCRYPTED` PEM headers (EVP_BytesToKey over MD5, CBC ciphers).

use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use der::{Decode, Encode};
use lumen_common::codec::{self, Padding};

use super::asn1::{self, Reader};
use super::model::{self, AsymKey, EcKey};
use super::{
    bad_decrypt, decoder_unsupported, interrupted, missing_passphrase, unknown_cipher, KResult,
    SendError,
};
use crate::crypto::cipher::bindings::bytes_to_key;

pub const FORMAT_DER: u32 = 0;
pub const FORMAT_PEM: u32 = 1;
pub const FORMAT_JWK: u32 = 2;
pub const ENC_PKCS1: u32 = 0;
pub const ENC_PKCS8: u32 = 1;
pub const ENC_SPKI: u32 = 2;
pub const ENC_SEC1: u32 = 3;

/// The largest passphrase OpenSSL's PEM password callback accepts.
const MAX_PASSPHRASE: usize = 1024;

/// One `-----BEGIN label-----` block: its RFC 1421 headers and decoded body.
pub struct PemBlock {
    pub label: String,
    pub headers: Vec<(String, String)>,
    pub data: Vec<u8>,
}

/// Every well-formed PEM block in `text`, in order (text outside blocks is skipped, as OpenSSL does).
pub fn pem_blocks(text: &[u8]) -> Vec<PemBlock> {
    let text = String::from_utf8_lossy(text);
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start + 11..];
        let Some(label_end) = after.find("-----") else {
            break;
        };
        let label = after[..label_end].to_string();
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(end) = body_start.find(&end_marker) else {
            rest = body_start;
            continue;
        };
        let body = &body_start[..end];
        rest = &body_start[end + end_marker.len()..];
        let mut headers = Vec::new();
        let mut b64 = String::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
                continue;
            }
            b64.push_str(line);
        }
        if let Ok(data) = codec::base64_decode_strict(b64.as_bytes(), false, Padding::Required) {
            out.push(PemBlock {
                label,
                headers,
                data,
            });
        }
    }
    out
}

/// PEM text with 64-column base64 lines.
pub fn pem_encode(label: &str, headers: &[(&str, String)], data: &[u8]) -> String {
    let b64 = codec::base64_encode(data, false, true);
    let mut out = format!("-----BEGIN {label}-----\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    if !headers.is_empty() {
        out.push('\n');
    }
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// A CBC cipher usable for key encryption, by OpenSSL name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCipher {
    Aes128Cbc,
    Aes192Cbc,
    Aes256Cbc,
    DesEde3Cbc,
    DesCbc,
}

impl KeyCipher {
    pub fn from_name(name: &str) -> Option<KeyCipher> {
        Some(match name.to_ascii_lowercase().as_str() {
            "aes-128-cbc" | "aes128" | "id-aes128-cbc" => KeyCipher::Aes128Cbc,
            "aes-192-cbc" | "aes192" | "id-aes192-cbc" => KeyCipher::Aes192Cbc,
            "aes-256-cbc" | "aes256" | "id-aes256-cbc" => KeyCipher::Aes256Cbc,
            "des-ede3-cbc" | "des3" | "des-ede3" => KeyCipher::DesEde3Cbc,
            "des-cbc" | "des" => KeyCipher::DesCbc,
            _ => return None,
        })
    }

    /// The name OpenSSL writes into `DEK-Info`.
    pub fn pem_name(self) -> &'static str {
        match self {
            KeyCipher::Aes128Cbc => "AES-128-CBC",
            KeyCipher::Aes192Cbc => "AES-192-CBC",
            KeyCipher::Aes256Cbc => "AES-256-CBC",
            KeyCipher::DesEde3Cbc => "DES-EDE3-CBC",
            KeyCipher::DesCbc => "DES-CBC",
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            KeyCipher::Aes128Cbc => 16,
            KeyCipher::Aes192Cbc | KeyCipher::DesEde3Cbc => 24,
            KeyCipher::Aes256Cbc => 32,
            KeyCipher::DesCbc => 8,
        }
    }

    pub fn iv_len(self) -> usize {
        match self {
            KeyCipher::DesEde3Cbc | KeyCipher::DesCbc => 8,
            _ => 16,
        }
    }

    fn encrypt(self, key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
        use cipher::block_padding::Pkcs7;
        macro_rules! enc {
            ($c:ty) => {
                cbc::Encryptor::<$c>::new_from_slices(key, iv)
                    .map(|e| e.encrypt_padded_vec_mut::<Pkcs7>(data))
                    .unwrap_or_default()
            };
        }
        match self {
            KeyCipher::Aes128Cbc => enc!(aes::Aes128),
            KeyCipher::Aes192Cbc => enc!(aes::Aes192),
            KeyCipher::Aes256Cbc => enc!(aes::Aes256),
            KeyCipher::DesEde3Cbc => enc!(des::TdesEde3),
            KeyCipher::DesCbc => enc!(des::Des),
        }
    }

    fn decrypt(self, key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
        use cipher::block_padding::Pkcs7;
        macro_rules! dec {
            ($c:ty) => {
                cbc::Decryptor::<$c>::new_from_slices(key, iv)
                    .ok()?
                    .decrypt_padded_vec_mut::<Pkcs7>(data)
                    .ok()
            };
        }
        match self {
            KeyCipher::Aes128Cbc => dec!(aes::Aes128),
            KeyCipher::Aes192Cbc => dec!(aes::Aes192),
            KeyCipher::Aes256Cbc => dec!(aes::Aes256),
            KeyCipher::DesEde3Cbc => dec!(des::TdesEde3),
            KeyCipher::DesCbc => dec!(des::Des),
        }
    }
}

fn random(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let _ = lumen_os::proc::entropy(&mut v);
    v
}

fn check_passphrase(passphrase: Option<&[u8]>) -> KResult<&[u8]> {
    match passphrase {
        Some(p) if p.len() <= MAX_PASSPHRASE => Ok(p),
        _ => Err(interrupted()),
    }
}

/// Decrypts a legacy encrypted PEM body (headers `Proc-Type: 4,ENCRYPTED` + `DEK-Info`).
fn legacy_decrypt(block: &PemBlock, passphrase: Option<&[u8]>) -> KResult<Option<Vec<u8>>> {
    let encrypted = block
        .headers
        .iter()
        .any(|(k, v)| k == "Proc-Type" && v.contains("ENCRYPTED"));
    if !encrypted {
        return Ok(None);
    }
    let dek = block
        .headers
        .iter()
        .find(|(k, _)| k == "DEK-Info")
        .map(|(_, v)| v.as_str())
        .ok_or_else(decoder_unsupported)?;
    let (name, iv_hex) = dek.split_once(',').ok_or_else(decoder_unsupported)?;
    let cipher = KeyCipher::from_name(name.trim()).ok_or_else(decoder_unsupported)?;
    let iv =
        codec::hex_decode_strict(iv_hex.trim().as_bytes()).map_err(|_| decoder_unsupported())?;
    if iv.len() != cipher.iv_len() {
        return Err(decoder_unsupported());
    }
    let pass = check_passphrase(passphrase)?;
    let key = bytes_to_key(pass, &iv[..8], cipher.key_len());
    cipher
        .decrypt(&key, &iv, &block.data)
        .map(Some)
        .ok_or_else(bad_decrypt)
}

/// Decrypts a DER `EncryptedPrivateKeyInfo`.
fn pkcs8_decrypt(der: &[u8], pass: &[u8]) -> KResult<Vec<u8>> {
    let info = pkcs8::EncryptedPrivateKeyInfo::from_der(der).map_err(|_| decoder_unsupported())?;
    let doc = info.decrypt(pass).map_err(|_| bad_decrypt())?;
    Ok(doc.as_bytes().to_vec())
}

/// Encrypts a DER `PrivateKeyInfo` as OpenSSL's `PEM_write_bio_PKCS8PrivateKey` does (PBES2,
/// PBKDF2-HMAC-SHA256 with 2048 iterations and a 16-byte salt).
fn pkcs8_encrypt(der: &[u8], cipher: KeyCipher, pass: &[u8]) -> KResult<Vec<u8>> {
    use pkcs5::pbes2;
    let salt = random(16);
    let iv = random(cipher.iv_len());
    let kdf = pbes2::Pbkdf2Params::hmac_with_sha256(2048, &salt)
        .map_err(|e| SendError::new("Error", e.to_string()))?;
    let iv8: [u8; 8] = iv[..8].try_into().unwrap_or_default();
    let iv16: [u8; 16] = if iv.len() == 16 {
        iv[..].try_into().unwrap_or_default()
    } else {
        [0; 16]
    };
    let encryption = match cipher {
        KeyCipher::Aes128Cbc => pbes2::EncryptionScheme::Aes128Cbc { iv: &iv16 },
        KeyCipher::Aes192Cbc => pbes2::EncryptionScheme::Aes192Cbc { iv: &iv16 },
        KeyCipher::Aes256Cbc => pbes2::EncryptionScheme::Aes256Cbc { iv: &iv16 },
        KeyCipher::DesEde3Cbc => pbes2::EncryptionScheme::DesEde3Cbc { iv: &iv8 },
        KeyCipher::DesCbc => pbes2::EncryptionScheme::DesCbc { iv: &iv8 },
    };
    let scheme = pkcs5::EncryptionScheme::from(pbes2::Parameters {
        kdf: kdf.into(),
        encryption,
    });
    let encrypted = scheme
        .encrypt(pass, der)
        .map_err(|e| SendError::new("Error", e.to_string()))?;
    let info = pkcs8::EncryptedPrivateKeyInfo {
        encryption_algorithm: scheme,
        encrypted_data: &encrypted,
    };
    info.to_der()
        .map_err(|e| SendError::new("Error", e.to_string()))
}

/// Whether DER bytes are a PKCS#1 `RSAPrivateKey` (as opposed to an `RSAPublicKey`): Node's
/// `IsRSAPrivateKey` heuristic (a leading one-byte version 0 or 1).
pub fn is_rsa_private_der(der: &[u8]) -> bool {
    match asn1::single(der).or_else(|| first_element(der)) {
        Some((asn1::TAG_SEQUENCE, body)) => {
            body.len() >= 3 && body[0] == 2 && body[1] == 1 && body[2] & 0xfe == 0
        }
        _ => false,
    }
}

/// Whether DER bytes are an `EncryptedPrivateKeyInfo` rather than a `PrivateKeyInfo`.
pub fn is_encrypted_pkcs8_der(der: &[u8]) -> bool {
    match first_element(der) {
        Some((asn1::TAG_SEQUENCE, body)) => !body.is_empty() && body[0] != asn1::TAG_INTEGER,
        _ => false,
    }
}

fn first_element(der: &[u8]) -> Option<(u8, &[u8])> {
    let mut r = Reader::new(der);
    r.read()
}

fn certificate_spki(der: &[u8]) -> KResult<AsymKey> {
    let cert = x509_cert::Certificate::from_der(der).map_err(|_| decoder_unsupported())?;
    let spki = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|_| decoder_unsupported())?;
    AsymKey::from_spki_der(&spki)
}

fn rsa_public_from_pkcs1(der: &[u8]) -> KResult<AsymKey> {
    let (n, e) = model::parse_pkcs1_public(der).ok_or_else(decoder_unsupported)?;
    Ok(AsymKey::Rsa(model::RsaKey {
        n,
        e,
        private: None,
        pss: None,
    }))
}

fn rsa_private_from_pkcs1(der: &[u8]) -> KResult<AsymKey> {
    model::parse_pkcs1_private(der, None)
        .map(AsymKey::Rsa)
        .ok_or_else(decoder_unsupported)
}

/// Parses a private key from a decrypted PEM block body.
fn private_from_block(label: &str, data: &[u8]) -> KResult<AsymKey> {
    match label {
        "PRIVATE KEY" => AsymKey::from_pkcs8_der(data),
        "RSA PRIVATE KEY" => rsa_private_from_pkcs1(data),
        "EC PRIVATE KEY" => model::parse_sec1(data, None).map(AsymKey::Ec),
        "DSA PRIVATE KEY" => model::parse_dsa_legacy(data)
            .map(AsymKey::Dsa)
            .ok_or_else(decoder_unsupported),
        _ => Err(decoder_unsupported()),
    }
}

const PRIVATE_LABELS: &[&str] = &[
    "PRIVATE KEY",
    "ENCRYPTED PRIVATE KEY",
    "RSA PRIVATE KEY",
    "EC PRIVATE KEY",
    "DSA PRIVATE KEY",
];

/// `ParsePrivateKey`: a private key from user input. `enc` is the `type` option (required for DER).
pub fn import_private(
    data: &[u8],
    format: u32,
    enc: Option<u32>,
    passphrase: Option<&[u8]>,
) -> KResult<AsymKey> {
    if format == FORMAT_PEM {
        let blocks = pem_blocks(data);
        let block = blocks
            .iter()
            .find(|b| PRIVATE_LABELS.contains(&b.label.as_str()))
            .ok_or_else(decoder_unsupported)?;
        if block.label == "ENCRYPTED PRIVATE KEY" {
            let pass = check_passphrase(passphrase)?;
            return AsymKey::from_pkcs8_der(&pkcs8_decrypt(&block.data, pass)?);
        }
        return match legacy_decrypt(block, passphrase)? {
            Some(plain) => private_from_block(&block.label, &plain),
            None => private_from_block(&block.label, &block.data),
        };
    }
    match enc {
        Some(ENC_PKCS1) => rsa_private_from_pkcs1(data),
        Some(ENC_SEC1) => model::parse_sec1(data, None).map(AsymKey::Ec),
        _ => {
            if is_encrypted_pkcs8_der(data) {
                let Some(pass) = passphrase else {
                    return Err(missing_passphrase());
                };
                return AsymKey::from_pkcs8_der(&pkcs8_decrypt(data, pass)?);
            }
            AsymKey::from_pkcs8_der(data)
        }
    }
}

/// `ParsePublicKeyPEM` / `ParsePublicKey`: a public key, from public or private input (a private
/// key yields its public half).
pub fn import_public(
    data: &[u8],
    format: u32,
    enc: Option<u32>,
    passphrase: Option<&[u8]>,
) -> KResult<AsymKey> {
    if format == FORMAT_PEM {
        let blocks = pem_blocks(data);
        if let Some(b) = blocks.iter().find(|b| b.label == "PUBLIC KEY") {
            return AsymKey::from_spki_der(&b.data);
        }
        if let Some(b) = blocks.iter().find(|b| b.label == "RSA PUBLIC KEY") {
            return rsa_public_from_pkcs1(&b.data);
        }
        if let Some(b) = blocks.iter().find(|b| {
            matches!(
                b.label.as_str(),
                "CERTIFICATE" | "X509 CERTIFICATE" | "TRUSTED CERTIFICATE"
            )
        }) {
            return certificate_spki(&b.data);
        }
        return import_private(data, format, enc, passphrase).map(|k| k.to_public());
    }
    match enc {
        Some(ENC_PKCS1) if !is_rsa_private_der(data) => rsa_public_from_pkcs1(data),
        Some(ENC_SPKI) => AsymKey::from_spki_der(data),
        _ => import_private(data, format, enc, passphrase).map(|k| k.to_public()),
    }
}

/// Output of an export: PEM text or DER bytes.
pub enum Exported {
    Pem(String),
    Der(Vec<u8>),
}

impl Exported {
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Exported::Pem(s) => s.into_bytes(),
            Exported::Der(d) => d,
        }
    }
}

fn wrap(format: u32, label: &str, der: Vec<u8>) -> Exported {
    if format == FORMAT_PEM {
        Exported::Pem(pem_encode(label, &[], &der))
    } else {
        Exported::Der(der)
    }
}

/// `WritePublicKey`: `enc` is `ENC_PKCS1` (RSA only) or `ENC_SPKI`.
pub fn export_public(key: &AsymKey, format: u32, enc: u32) -> KResult<Exported> {
    if enc == ENC_PKCS1 {
        let AsymKey::Rsa(k) = key else {
            return Err(SendError::new("Error", "Failed to encode public key"));
        };
        return Ok(wrap(format, "RSA PUBLIC KEY", k.pkcs1_public_der()));
    }
    Ok(wrap(format, "PUBLIC KEY", key.to_spki_der()))
}

/// Validates an export cipher name (`ERR_CRYPTO_UNKNOWN_CIPHER` otherwise).
pub fn export_cipher(name: Option<&str>) -> KResult<Option<KeyCipher>> {
    match name {
        None => Ok(None),
        Some(n) => KeyCipher::from_name(n).map(Some).ok_or_else(unknown_cipher),
    }
}

fn legacy_pem(
    label: &str,
    der: &[u8],
    cipher: Option<KeyCipher>,
    pass: Option<&[u8]>,
) -> KResult<Exported> {
    let Some(cipher) = cipher else {
        return Ok(Exported::Pem(pem_encode(label, &[], der)));
    };
    let pass = check_passphrase(pass)?;
    let iv = random(cipher.iv_len());
    let key = bytes_to_key(pass, &iv[..8], cipher.key_len());
    let body = cipher.encrypt(&key, &iv, der);
    let headers = [
        ("Proc-Type", "4,ENCRYPTED".to_string()),
        (
            "DEK-Info",
            format!("{},{}", cipher.pem_name(), codec::hex_encode_upper(&iv)),
        ),
    ];
    Ok(Exported::Pem(pem_encode(label, &headers, &body)))
}

/// `WritePrivateKey`: `enc` is `ENC_PKCS1` (RSA), `ENC_PKCS8` or `ENC_SEC1` (EC).
pub fn export_private(
    key: &AsymKey,
    format: u32,
    enc: u32,
    cipher: Option<&str>,
    pass: Option<&[u8]>,
) -> KResult<Exported> {
    let cipher = export_cipher(cipher)?;
    let fail = || SendError::new("Error", "Failed to encode private key");
    match enc {
        ENC_PKCS1 => {
            let AsymKey::Rsa(k) = key else {
                return Err(fail());
            };
            let der = k.pkcs1_private_der().ok_or_else(fail)?;
            if format == FORMAT_PEM {
                legacy_pem("RSA PRIVATE KEY", &der, cipher, pass)
            } else {
                Ok(Exported::Der(der))
            }
        }
        ENC_SEC1 => {
            let AsymKey::Ec(k) = key else {
                return Err(fail());
            };
            let der = sec1_der(k).ok_or_else(fail)?;
            if format == FORMAT_PEM {
                legacy_pem("EC PRIVATE KEY", &der, cipher, pass)
            } else {
                Ok(Exported::Der(der))
            }
        }
        _ => {
            let der = key.to_pkcs8_der()?;
            match cipher {
                None => Ok(wrap(format, "PRIVATE KEY", der)),
                Some(c) => {
                    let pass = pass.unwrap_or_default();
                    if pass.len() > MAX_PASSPHRASE {
                        return Err(interrupted());
                    }
                    Ok(wrap(
                        format,
                        "ENCRYPTED PRIVATE KEY",
                        pkcs8_encrypt(&der, c, pass)?,
                    ))
                }
            }
        }
    }
}

fn sec1_der(k: &EcKey) -> Option<Vec<u8>> {
    k.sec1_der(true)
}
