//! The neutral key and scheme types that cross the [`Backend`](crate::Backend) trait. Integers are
//! big-endian byte strings.

use crate::Algo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RsaPublicKey {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

/// A private key with its CRT values (`dp = d mod p-1`, `dq = d mod q-1`, `qi = q^-1 mod p`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RsaPrivateKey {
    pub public: RsaPublicKey,
    pub d: Vec<u8>,
    pub p: Vec<u8>,
    pub q: Vec<u8>,
    pub dp: Vec<u8>,
    pub dq: Vec<u8>,
    pub qi: Vec<u8>,
}

/// RSA padding for encryption and for the raw private-key operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RsaPadding {
    /// `RSA_PKCS1_PADDING`.
    Pkcs1,
    /// `RSA_NO_PADDING`.
    None,
    /// `RSA_PKCS1_OAEP_PADDING` with the label hash `hash`, the MGF1 digest `mgf1` and `label`.
    Oaep { hash: Algo, mgf1: Algo, label: Vec<u8> },
}

/// How long the salt of an RSASSA-PSS signature is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PssSalt {
    /// `RSA_PSS_SALTLEN_DIGEST`: as long as the digest.
    Digest,
    /// The largest salt that fits when signing; any length that verifies when verifying
    /// (`RSA_PSS_SALTLEN_AUTO`).
    MaxOrAuto,
    Length(u32),
}

/// A signature scheme; `hash` is the digest the caller computed the message digest with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RsaScheme {
    /// PKCS#1 v1.5 with a `DigestInfo` for `hash` (`Algo::Md5Sha1` signs the bare digest).
    Pkcs1 { hash: Algo },
    Pss { hash: Algo, mgf1: Algo, salt: PssSalt },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DsaParams {
    pub p: Vec<u8>,
    pub q: Vec<u8>,
    pub g: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DsaPublicKey {
    pub params: DsaParams,
    pub y: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DsaPrivateKey {
    pub public: DsaPublicKey,
    pub x: Vec<u8>,
}

/// Finite-field Diffie-Hellman parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhParams {
    pub p: Vec<u8>,
    pub g: Vec<u8>,
}

pub const DH_CHECK_P_NOT_PRIME: u32 = 0x01;
pub const DH_CHECK_P_NOT_SAFE_PRIME: u32 = 0x02;
pub const DH_UNABLE_TO_CHECK_GENERATOR: u32 = 0x04;
pub const DH_NOT_SUITABLE_GENERATOR: u32 = 0x08;
pub const DH_MODULUS_TOO_SMALL: u32 = 0x80;
pub const DH_MODULUS_TOO_LARGE: u32 = 0x100;

pub const DH_CHECK_PUBKEY_TOO_SMALL: u32 = 0x01;
pub const DH_CHECK_PUBKEY_TOO_LARGE: u32 = 0x02;
pub const DH_CHECK_PUBKEY_INVALID: u32 = 0x04;

/// Big-endian bytes of `n`, left-padded with zeros to `len` (without leading zeros when longer).
pub fn pad_be(n: &[u8], len: usize) -> Vec<u8> {
    let n = trim_be(n);
    if n.len() >= len {
        return n.to_vec();
    }
    let mut out = vec![0; len - n.len()];
    out.extend_from_slice(n);
    out
}

/// `n` without its leading zero bytes.
pub fn trim_be(n: &[u8]) -> &[u8] {
    let start = n.iter().position(|b| *b != 0).unwrap_or(n.len());
    &n[start..]
}
