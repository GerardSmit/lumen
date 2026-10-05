//! The asymmetric key model: parsing the normalized DER forms a `KeyObjectHandle` stores (SPKI for
//! public keys, PKCS#8 for private keys), writing them back, deriving public keys and describing
//! keys the way OpenSSL does.

use der::asn1::ObjectIdentifier;
use num_bigint_dig::BigUint;

use super::asn1::{self, Reader, TAG_NULL, TAG_OCTET_STRING, TAG_OID, TAG_SEQUENCE};
use super::curves::EcCurve;
use super::{decoder_unsupported, invalid_private, KResult};

/// `g^x mod p`, the public value of a DH or DSA private key.
fn exp_public(p: &BigUint, g: &BigUint, x: &BigUint) -> Option<BigUint> {
    let params = lumen_crypto::DhParams {
        p: p.to_bytes_be(),
        g: g.to_bytes_be(),
    };
    let y = lumen_crypto::backend()
        .dh_public(&params, &x.to_bytes_be())
        .ok()?;
    Some(BigUint::from_bytes_be(&y))
}

/// An asymmetric key, public or private (`is_private`).
#[derive(Clone, Debug)]
pub enum AsymKey {
    Rsa(RsaKey),
    Dsa(DsaKey),
    Ec(EcKey),
    Ed25519(OkpKey),
    Ed448(OkpKey),
    X25519(OkpKey),
    X448(OkpKey),
    Dh(DhKey),
}

/// An RSA or RSASSA-PSS key. `pss` is `None` for `rsaEncryption` keys; for `rsa-pss` keys it holds
/// the parameter restrictions, if the key carries any.
#[derive(Clone, Debug)]
pub struct RsaKey {
    pub n: BigUint,
    pub e: BigUint,
    pub private: Option<RsaPrivateParts>,
    pub pss: Option<Option<PssParams>>,
}

#[derive(Clone, Debug)]
pub struct RsaPrivateParts {
    pub d: BigUint,
    pub p: BigUint,
    pub q: BigUint,
    pub dp: BigUint,
    pub dq: BigUint,
    pub qi: BigUint,
}

/// RSASSA-PSS-params with defaults applied. Hash names are OpenSSL's (`sha256`, `sha512-224`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PssParams {
    pub hash: &'static str,
    pub mgf1_hash: &'static str,
    pub salt_length: u32,
}

#[derive(Clone, Debug)]
pub struct DsaKey {
    pub p: BigUint,
    pub q: BigUint,
    pub g: BigUint,
    pub y: BigUint,
    pub x: Option<BigUint>,
}

/// An EC key on a named curve. `point` is the uncompressed SEC1 public point, `d` the private
/// scalar as big-endian bytes of the curve's field length. `explicit` keys write their curve as
/// explicit `ECParameters` rather than a named-curve OID.
#[derive(Clone, Debug)]
pub struct EcKey {
    pub curve: EcCurve,
    pub point: Vec<u8>,
    pub d: Option<Vec<u8>>,
    pub explicit: bool,
}

/// An Ed25519 / Ed448 / X25519 / X448 key: the raw RFC 8032 / RFC 7748 encodings.
#[derive(Clone, Debug)]
pub struct OkpKey {
    pub public: Vec<u8>,
    pub private: Option<Vec<u8>>,
}

/// A finite-field Diffie-Hellman key (PKCS#3 `dhKeyAgreement`, or X9.42 when `q` is known).
#[derive(Clone, Debug)]
pub struct DhKey {
    pub p: BigUint,
    pub g: BigUint,
    pub q: Option<BigUint>,
    pub y: BigUint,
    pub x: Option<BigUint>,
}

/// `(OpenSSL name, OID, output length)` of the digests RSASSA-PSS parameters may name.
const PSS_HASHES: &[(&str, &str, u32)] = &[
    ("sha1", "1.3.14.3.2.26", 20),
    ("sha224", "2.16.840.1.101.3.4.2.4", 28),
    ("sha256", "2.16.840.1.101.3.4.2.1", 32),
    ("sha384", "2.16.840.1.101.3.4.2.2", 48),
    ("sha512", "2.16.840.1.101.3.4.2.3", 64),
    ("sha512-224", "2.16.840.1.101.3.4.2.5", 28),
    ("sha512-256", "2.16.840.1.101.3.4.2.6", 32),
    ("sha3-224", "2.16.840.1.101.3.4.2.7", 28),
    ("sha3-256", "2.16.840.1.101.3.4.2.8", 32),
    ("sha3-384", "2.16.840.1.101.3.4.2.9", 48),
    ("sha3-512", "2.16.840.1.101.3.4.2.10", 64),
    ("md5", "1.2.840.113549.2.5", 16),
];

/// The OpenSSL name of a digest name OpenSSL accepts for RSA-PSS keys (`SHA256`, `RSA-SHA256`,
/// `sha-256` ...), with its output length.
pub fn pss_hash(name: &str) -> Option<(&'static str, u32)> {
    let lower = name.to_ascii_lowercase();
    let lower = lower.strip_prefix("rsa-").unwrap_or(&lower);
    let canon = match lower {
        "sha-1" => "sha1",
        "sha-224" => "sha224",
        "sha-256" => "sha256",
        "sha-384" => "sha384",
        "sha-512" => "sha512",
        "sha-512/224" | "sha512/224" => "sha512-224",
        "sha-512/256" | "sha512/256" => "sha512-256",
        other => other,
    };
    PSS_HASHES.iter().find(|h| h.0 == canon).map(|h| (h.0, h.2))
}

fn hash_by_oid(oid: &ObjectIdentifier) -> Option<&'static str> {
    PSS_HASHES
        .iter()
        .find(|h| ObjectIdentifier::new(h.1).ok().as_ref() == Some(oid))
        .map(|h| h.0)
}

fn hash_oid(name: &str) -> ObjectIdentifier {
    let oid = PSS_HASHES
        .iter()
        .find(|h| h.0 == name)
        .map_or("1.3.14.3.2.26", |h| h.1);
    ObjectIdentifier::new_unwrap(oid)
}

impl PssParams {
    pub const DEFAULT: PssParams = PssParams {
        hash: "sha1",
        mgf1_hash: "sha1",
        salt_length: 20,
    };

    /// Parses an `RSASSA-PSS-params` SEQUENCE (content bytes).
    pub fn parse(body: &[u8]) -> Option<PssParams> {
        let mut r = Reader::new(body);
        let mut out = PssParams::DEFAULT;
        if let Some(c) = r.optional(0xa0) {
            out.hash = hash_by_oid(&Reader::new(c).sequence()?.oid()?)?;
        }
        if let Some(c) = r.optional(0xa1) {
            let mut alg = Reader::new(c).sequence()?;
            if alg.oid()? != asn1::OID_MGF1 {
                return None;
            }
            out.mgf1_hash = hash_by_oid(&alg.sequence()?.oid()?)?;
        }
        if let Some(c) = r.optional(0xa2) {
            out.salt_length = u32::try_from(Reader::new(c).small_uint()?).ok()?;
        }
        if let Some(c) = r.optional(0xa3) {
            if Reader::new(c).small_uint()? != 1 {
                return None;
            }
        }
        r.finish()?;
        Some(out)
    }

    /// The DER `RSASSA-PSS-params` SEQUENCE, with default fields omitted.
    pub fn to_der(&self) -> Vec<u8> {
        let mut body = Vec::new();
        if self.hash != "sha1" {
            body.extend(asn1::explicit(
                0,
                &asn1::seq(&[&asn1::oid(&hash_oid(self.hash))]),
            ));
        }
        if self.mgf1_hash != "sha1" {
            let inner = asn1::seq(&[&asn1::oid(&hash_oid(self.mgf1_hash))]);
            body.extend(asn1::explicit(
                1,
                &asn1::seq(&[&asn1::oid(&asn1::OID_MGF1), &inner]),
            ));
        }
        if self.salt_length != 20 {
            body.extend(asn1::explicit(
                2,
                &asn1::small_uint(self.salt_length as u64),
            ));
        }
        asn1::tlv(TAG_SEQUENCE, &body)
    }
}

impl AsymKey {
    /// Parses a DER `SubjectPublicKeyInfo`.
    pub fn from_spki_der(der: &[u8]) -> KResult<AsymKey> {
        parse_spki(der).ok_or_else(decoder_unsupported)?
    }

    /// Parses a DER (unencrypted) `PrivateKeyInfo` / `OneAsymmetricKey`.
    pub fn from_pkcs8_der(der: &[u8]) -> KResult<AsymKey> {
        parse_pkcs8(der).ok_or_else(decoder_unsupported)?
    }

    /// Parses what a `KeyObjectHandle` stores: SPKI for `kind` 1 (public), PKCS#8 for 2 (private).
    pub fn from_handle(kind: u32, der: &[u8]) -> KResult<AsymKey> {
        if kind == 2 {
            AsymKey::from_pkcs8_der(der)
        } else {
            AsymKey::from_spki_der(der)
        }
    }

    pub fn is_private(&self) -> bool {
        match self {
            AsymKey::Rsa(k) => k.private.is_some(),
            AsymKey::Dsa(k) => k.x.is_some(),
            AsymKey::Ec(k) => k.d.is_some(),
            AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
                k.private.is_some()
            }
            AsymKey::Dh(k) => k.x.is_some(),
        }
    }

    /// `asymmetricKeyType`: `rsa`, `rsa-pss`, `dsa`, `ec`, `ed25519`, `ed448`, `x25519`, `x448`, `dh`.
    pub fn type_name(&self) -> &'static str {
        match self {
            AsymKey::Rsa(k) if k.pss.is_some() => "rsa-pss",
            AsymKey::Rsa(_) => "rsa",
            AsymKey::Dsa(_) => "dsa",
            AsymKey::Ec(_) => "ec",
            AsymKey::Ed25519(_) => "ed25519",
            AsymKey::Ed448(_) => "ed448",
            AsymKey::X25519(_) => "x25519",
            AsymKey::X448(_) => "x448",
            AsymKey::Dh(_) => "dh",
        }
    }

    /// The public half.
    pub fn to_public(&self) -> AsymKey {
        let mut k = self.clone();
        match &mut k {
            AsymKey::Rsa(k) => k.private = None,
            AsymKey::Dsa(k) => k.x = None,
            AsymKey::Ec(k) => k.d = None,
            AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
                k.private = None
            }
            AsymKey::Dh(k) => k.x = None,
        }
        k
    }

    fn algorithm_identifier(&self) -> Vec<u8> {
        match self {
            AsymKey::Rsa(k) => match &k.pss {
                None => asn1::seq(&[&asn1::oid(&asn1::OID_RSA), &asn1::null()]),
                Some(None) => asn1::seq(&[&asn1::oid(&asn1::OID_RSA_PSS)]),
                Some(Some(p)) => asn1::seq(&[&asn1::oid(&asn1::OID_RSA_PSS), &p.to_der()]),
            },
            AsymKey::Dsa(k) => asn1::seq(&[&asn1::oid(&asn1::OID_DSA), &k.params_der()]),
            AsymKey::Ec(k) => {
                let params = if k.explicit {
                    k.curve.explicit_params()
                } else {
                    asn1::oid(&k.curve.oid())
                };
                asn1::seq(&[&asn1::oid(&asn1::OID_EC), &params])
            }
            AsymKey::Ed25519(_) => asn1::seq(&[&asn1::oid(&asn1::OID_ED25519)]),
            AsymKey::Ed448(_) => asn1::seq(&[&asn1::oid(&asn1::OID_ED448)]),
            AsymKey::X25519(_) => asn1::seq(&[&asn1::oid(&asn1::OID_X25519)]),
            AsymKey::X448(_) => asn1::seq(&[&asn1::oid(&asn1::OID_X448)]),
            AsymKey::Dh(k) => match &k.q {
                Some(q) => asn1::seq(&[
                    &asn1::oid(&asn1::OID_DHX),
                    &asn1::seq(&[
                        &asn1::biguint(&k.p),
                        &asn1::biguint(&k.g),
                        &asn1::biguint(q),
                    ]),
                ]),
                None => asn1::seq(&[
                    &asn1::oid(&asn1::OID_DH),
                    &asn1::seq(&[&asn1::biguint(&k.p), &asn1::biguint(&k.g)]),
                ]),
            },
        }
    }

    /// The DER `SubjectPublicKeyInfo` of the (public half of the) key.
    pub fn to_spki_der(&self) -> Vec<u8> {
        let key = match self {
            AsymKey::Rsa(k) => k.pkcs1_public_der(),
            AsymKey::Dsa(k) => asn1::biguint(&k.y),
            AsymKey::Ec(k) => k.point.clone(),
            AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
                k.public.clone()
            }
            AsymKey::Dh(k) => asn1::biguint(&k.y),
        };
        asn1::seq(&[&self.algorithm_identifier(), &asn1::bit_string(&key)])
    }

    /// The DER `PrivateKeyInfo` (version 0, as OpenSSL writes it). Fails for a public key.
    pub fn to_pkcs8_der(&self) -> KResult<Vec<u8>> {
        let inner = match self {
            AsymKey::Rsa(k) => k.pkcs1_private_der().ok_or_else(not_private)?,
            AsymKey::Dsa(k) => asn1::biguint(k.x.as_ref().ok_or_else(not_private)?),
            AsymKey::Ec(k) => k.sec1_der(false).ok_or_else(not_private)?,
            AsymKey::Ed25519(k) | AsymKey::Ed448(k) | AsymKey::X25519(k) | AsymKey::X448(k) => {
                asn1::octets(k.private.as_ref().ok_or_else(not_private)?)
            }
            AsymKey::Dh(k) => asn1::biguint(k.x.as_ref().ok_or_else(not_private)?),
        };
        Ok(asn1::seq(&[
            &asn1::small_uint(0),
            &self.algorithm_identifier(),
            &asn1::octets(&inner),
        ]))
    }

    /// The normalized form a `KeyObjectHandle` stores for this key.
    pub fn to_handle_der(&self) -> Vec<u8> {
        if self.is_private() {
            self.to_pkcs8_der().unwrap_or_default()
        } else {
            self.to_spki_der()
        }
    }

    /// Whether two keys are the same key (OpenSSL's `EVP_PKEY_eq`: type, parameters and public
    /// components; a private key equals another private key with the same public half).
    pub fn public_eq(&self, other: &AsymKey) -> bool {
        self.to_spki_der() == other.to_spki_der()
    }
}

fn not_private() -> super::SendError {
    super::SendError::new("Error", "key is not a private key")
}

impl RsaKey {
    /// DER `RSAPublicKey` (PKCS#1).
    pub fn pkcs1_public_der(&self) -> Vec<u8> {
        asn1::seq(&[&asn1::biguint(&self.n), &asn1::biguint(&self.e)])
    }

    /// DER `RSAPrivateKey` (PKCS#1), for a private key.
    pub fn pkcs1_private_der(&self) -> Option<Vec<u8>> {
        let p = self.private.as_ref()?;
        Some(asn1::seq(&[
            &asn1::small_uint(0),
            &asn1::biguint(&self.n),
            &asn1::biguint(&self.e),
            &asn1::biguint(&p.d),
            &asn1::biguint(&p.p),
            &asn1::biguint(&p.q),
            &asn1::biguint(&p.dp),
            &asn1::biguint(&p.dq),
            &asn1::biguint(&p.qi),
        ]))
    }

    pub fn modulus_bits(&self) -> usize {
        self.n.bits()
    }

    /// The public half in the form `lumen_crypto` takes.
    pub fn public_parts(&self) -> lumen_crypto::RsaPublicKey {
        lumen_crypto::RsaPublicKey {
            n: self.n.to_bytes_be(),
            e: self.e.to_bytes_be(),
        }
    }

    /// The private key in the form `lumen_crypto` takes.
    pub fn private_parts(&self) -> KResult<lumen_crypto::RsaPrivateKey> {
        let p = self.private.as_ref().ok_or_else(not_private)?;
        Ok(lumen_crypto::RsaPrivateKey {
            public: self.public_parts(),
            d: p.d.to_bytes_be(),
            p: p.p.to_bytes_be(),
            q: p.q.to_bytes_be(),
            dp: p.dp.to_bytes_be(),
            dq: p.dq.to_bytes_be(),
            qi: p.qi.to_bytes_be(),
        })
    }

    /// Builds the model from a generated `lumen_crypto` private key.
    pub fn from_parts(k: &lumen_crypto::RsaPrivateKey, pss: Option<Option<PssParams>>) -> RsaKey {
        let num = |b: &[u8]| BigUint::from_bytes_be(b);
        RsaKey {
            n: num(&k.public.n),
            e: num(&k.public.e),
            private: Some(RsaPrivateParts {
                d: num(&k.d),
                p: num(&k.p),
                q: num(&k.q),
                dp: num(&k.dp),
                dq: num(&k.dq),
                qi: num(&k.qi),
            }),
            pss,
        }
    }
}

impl DsaKey {
    /// DER `Dss-Parms` SEQUENCE.
    pub fn params_der(&self) -> Vec<u8> {
        asn1::seq(&[
            &asn1::biguint(&self.p),
            &asn1::biguint(&self.q),
            &asn1::biguint(&self.g),
        ])
    }

    /// The public half in the form `lumen_crypto` takes.
    pub fn public_parts(&self) -> lumen_crypto::DsaPublicKey {
        lumen_crypto::DsaPublicKey {
            params: lumen_crypto::DsaParams {
                p: self.p.to_bytes_be(),
                q: self.q.to_bytes_be(),
                g: self.g.to_bytes_be(),
            },
            y: self.y.to_bytes_be(),
        }
    }

    /// The private key in the form `lumen_crypto` takes.
    pub fn private_parts(&self) -> KResult<lumen_crypto::DsaPrivateKey> {
        let x = self.x.as_ref().ok_or_else(not_private)?;
        Ok(lumen_crypto::DsaPrivateKey {
            public: self.public_parts(),
            x: x.to_bytes_be(),
        })
    }

    /// Builds the model from a generated `lumen_crypto` key.
    pub fn from_parts(k: &lumen_crypto::DsaPrivateKey) -> DsaKey {
        let num = |b: &[u8]| BigUint::from_bytes_be(b);
        DsaKey {
            p: num(&k.public.params.p),
            q: num(&k.public.params.q),
            g: num(&k.public.params.g),
            y: num(&k.public.y),
            x: Some(num(&k.x)),
        }
    }
}

impl EcKey {
    /// DER `ECPrivateKey` (SEC1). `with_params` adds the `[0]` curve parameters, as the
    /// standalone SEC1 encoding has them; PKCS#8 leaves them to the algorithm identifier.
    pub fn sec1_der(&self, with_params: bool) -> Option<Vec<u8>> {
        let d = self.d.as_ref()?;
        let mut parts: Vec<Vec<u8>> = vec![asn1::small_uint(1), asn1::octets(d)];
        if with_params {
            let params = if self.explicit {
                self.curve.explicit_params()
            } else {
                asn1::oid(&self.curve.oid())
            };
            parts.push(asn1::explicit(0, &params));
        }
        parts.push(asn1::explicit(1, &asn1::bit_string(&self.point)));
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        Some(asn1::seq(&refs))
    }

    /// The public key as an `elliptic_curve` key of curve `C` (which must be `self.curve`).
    pub fn public_key<C>(&self) -> KResult<elliptic_curve::PublicKey<C>>
    where
        C: elliptic_curve::CurveArithmetic,
        elliptic_curve::FieldBytesSize<C>: elliptic_curve::sec1::ModulusSize,
        elliptic_curve::AffinePoint<C>:
            elliptic_curve::sec1::FromEncodedPoint<C> + elliptic_curve::sec1::ToEncodedPoint<C>,
    {
        elliptic_curve::PublicKey::<C>::from_sec1_bytes(&self.point)
            .map_err(|_| super::invalid_point())
    }

    /// The private key as an `elliptic_curve` secret key of curve `C` (which must be `self.curve`).
    pub fn secret_key<C>(&self) -> KResult<elliptic_curve::SecretKey<C>>
    where
        C: elliptic_curve::CurveArithmetic,
    {
        let d = self.d.as_ref().ok_or_else(not_private)?;
        elliptic_curve::SecretKey::<C>::from_slice(d).map_err(|_| invalid_private())
    }

    /// The private scalar as a `p256` key (for P-256 keys).
    pub fn p256_secret(&self) -> KResult<p256::SecretKey> {
        self.secret_key::<p256::NistP256>()
    }
}

impl OkpKey {
    /// The private key as a fixed-size array (`N` = 32 for Ed25519/X25519, 57 for Ed448, 56 for X448).
    pub fn private_array<const N: usize>(&self) -> KResult<[u8; N]> {
        self.private
            .as_deref()
            .ok_or_else(not_private)?
            .try_into()
            .map_err(|_| invalid_private())
    }

    pub fn public_array<const N: usize>(&self) -> KResult<[u8; N]> {
        self.public
            .as_slice()
            .try_into()
            .map_err(|_| decoder_unsupported())
    }

    pub fn ed25519_signing(&self) -> KResult<ed25519_dalek::SigningKey> {
        Ok(ed25519_dalek::SigningKey::from_bytes(
            &self.private_array::<32>()?,
        ))
    }

    pub fn ed25519_verifying(&self) -> KResult<ed25519_dalek::VerifyingKey> {
        ed25519_dalek::VerifyingKey::from_bytes(&self.public_array::<32>()?)
            .map_err(|_| decoder_unsupported())
    }

    pub fn ed448_signing(&self) -> KResult<ed448_goldilocks_plus::SigningKey> {
        ed448_goldilocks_plus::SigningKey::try_from(
            self.private.as_deref().ok_or_else(not_private)?,
        )
        .map_err(|_| invalid_private())
    }

    pub fn ed448_verifying(&self) -> KResult<ed448_goldilocks_plus::VerifyingKey> {
        ed448_goldilocks_plus::VerifyingKey::from_bytes(&self.public_array::<57>()?)
            .map_err(|_| decoder_unsupported())
    }

    pub fn x25519_secret(&self) -> KResult<x25519_dalek::StaticSecret> {
        Ok(x25519_dalek::StaticSecret::from(
            self.private_array::<32>()?,
        ))
    }

    pub fn x25519_public(&self) -> KResult<x25519_dalek::PublicKey> {
        Ok(x25519_dalek::PublicKey::from(self.public_array::<32>()?))
    }
}

impl DhKey {
    /// DER `DHParameter` (PKCS#3) or X9.42 `DomainParameters` when `q` is known.
    pub fn params_der(&self) -> Vec<u8> {
        match &self.q {
            Some(q) => asn1::seq(&[
                &asn1::biguint(&self.p),
                &asn1::biguint(&self.g),
                &asn1::biguint(q),
            ]),
            None => asn1::seq(&[&asn1::biguint(&self.p), &asn1::biguint(&self.g)]),
        }
    }
}

/// The public value of an Ed25519 / Ed448 / X25519 / X448 private key.
pub fn okp_public(kind: &str, private: &[u8]) -> KResult<Vec<u8>> {
    match kind {
        "ed25519" => {
            let seed: [u8; 32] = private.try_into().map_err(|_| invalid_private())?;
            Ok(ed25519_dalek::SigningKey::from_bytes(&seed)
                .verifying_key()
                .to_bytes()
                .to_vec())
        }
        "ed448" => {
            let sk = ed448_goldilocks_plus::SigningKey::try_from(private)
                .map_err(|_| invalid_private())?;
            Ok(sk.verifying_key().to_bytes().to_vec())
        }
        "x25519" => {
            let k: [u8; 32] = private.try_into().map_err(|_| invalid_private())?;
            Ok(
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(k))
                    .to_bytes()
                    .to_vec(),
            )
        }
        "x448" => {
            let k: [u8; 56] = private.try_into().map_err(|_| invalid_private())?;
            Ok(x448_mul(&k, &ed448_goldilocks_plus::MontgomeryPoint::GENERATOR.0).to_vec())
        }
        _ => Err(decoder_unsupported()),
    }
}

/// X448(k, u) (RFC 7748 §5): the scalar is decoded with the RFC's clamping, the ladder is the
/// crate's.
pub fn x448_mul(k: &[u8; 56], u: &[u8; 56]) -> [u8; 56] {
    let mut s = *k;
    s[0] &= 252;
    s[55] |= 128;
    let scalar = ed448_goldilocks_plus::Scalar::from_bytes(&s);
    (&ed448_goldilocks_plus::MontgomeryPoint(*u) * &scalar).0
}

fn parse_alg(r: &mut Reader<'_>) -> Option<(ObjectIdentifier, Option<(u8, Vec<u8>)>)> {
    let mut alg = r.sequence()?;
    let oid = alg.oid()?;
    let params = if alg.is_empty() {
        None
    } else {
        let raw = alg.read_raw()?;
        Some((raw[0], raw.to_vec()))
    };
    alg.finish()?;
    Some((oid, params))
}

fn rsa_variant(
    oid: &ObjectIdentifier,
    params: &Option<(u8, Vec<u8>)>,
) -> Option<Option<Option<PssParams>>> {
    if *oid == asn1::OID_RSA {
        return match params {
            None => Some(None),
            Some((TAG_NULL, _)) => Some(None),
            _ => None,
        };
    }
    if *oid == asn1::OID_RSA_PSS {
        return match params {
            None => Some(Some(None)),
            Some((TAG_SEQUENCE, raw)) => {
                let (_, body) = asn1::single(raw)?;
                Some(Some(Some(PssParams::parse(body)?)))
            }
            _ => None,
        };
    }
    None
}

fn ec_params(params: &Option<(u8, Vec<u8>)>) -> Option<(EcCurve, bool)> {
    match params {
        Some((TAG_OID, raw)) => {
            let (_, c) = asn1::single(raw)?;
            Some((
                EcCurve::from_oid(&ObjectIdentifier::from_bytes(c).ok()?)?,
                false,
            ))
        }
        Some((TAG_SEQUENCE, raw)) => Some((EcCurve::from_explicit(raw)?, true)),
        _ => None,
    }
}

fn three_ints(raw: &[u8]) -> Option<(BigUint, BigUint, BigUint)> {
    let (tag, body) = asn1::single(raw)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    let out = (r.biguint()?, r.biguint()?, r.biguint()?);
    r.finish()?;
    Some(out)
}

/// `(p, g, q)` of PKCS#3 `DHParameter` (`q` absent) or X9.42 `DomainParameters`.
fn dh_params(
    oid: &ObjectIdentifier,
    params: &Option<(u8, Vec<u8>)>,
) -> Option<(BigUint, BigUint, Option<BigUint>)> {
    let (_, raw) = params.as_ref()?;
    let (tag, body) = asn1::single(raw)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    let p = r.biguint()?;
    let g = r.biguint()?;
    if *oid == asn1::OID_DHX {
        let q = r.biguint()?;
        return Some((p, g, Some(q)));
    }
    Some((p, g, None))
}

fn okp_ctor(oid: &ObjectIdentifier) -> Option<(fn(OkpKey) -> AsymKey, &'static str, usize)> {
    if *oid == asn1::OID_ED25519 {
        Some((AsymKey::Ed25519, "ed25519", 32))
    } else if *oid == asn1::OID_ED448 {
        Some((AsymKey::Ed448, "ed448", 57))
    } else if *oid == asn1::OID_X25519 {
        Some((AsymKey::X25519, "x25519", 32))
    } else if *oid == asn1::OID_X448 {
        Some((AsymKey::X448, "x448", 56))
    } else {
        None
    }
}

/// Parses a PKCS#1 `RSAPublicKey`.
pub fn parse_pkcs1_public(der: &[u8]) -> Option<(BigUint, BigUint)> {
    let (tag, body) = asn1::single(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    let n = r.biguint()?;
    let e = r.biguint()?;
    r.finish()?;
    Some((n, e))
}

/// Parses a PKCS#1 `RSAPrivateKey` (two-prime).
pub fn parse_pkcs1_private(der: &[u8], pss: Option<Option<PssParams>>) -> Option<RsaKey> {
    let (tag, body) = asn1::single(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    if r.small_uint()? != 0 {
        return None;
    }
    let n = r.biguint()?;
    let e = r.biguint()?;
    let d = r.biguint()?;
    let p = r.biguint()?;
    let q = r.biguint()?;
    let dp = r.biguint()?;
    let dq = r.biguint()?;
    let qi = r.biguint()?;
    r.finish()?;
    Some(RsaKey {
        n,
        e,
        private: Some(RsaPrivateParts {
            d,
            p,
            q,
            dp,
            dq,
            qi,
        }),
        pss,
    })
}

/// Parses a SEC1 `ECPrivateKey`; `curve` comes from the enclosing structure when it has one.
pub fn parse_sec1(der: &[u8], outer: Option<(EcCurve, bool)>) -> KResult<EcKey> {
    let bad = decoder_unsupported;
    let (tag, body) = asn1::single(der).ok_or_else(bad)?;
    if tag != TAG_SEQUENCE {
        return Err(bad());
    }
    let mut r = Reader::new(body);
    if r.small_uint().ok_or_else(bad)? != 1 {
        return Err(bad());
    }
    let d = r.expect(TAG_OCTET_STRING).ok_or_else(bad)?.to_vec();
    let mut curve = outer;
    if let Some(c) = r.optional(0xa0) {
        let mut pr = Reader::new(c);
        let raw = pr.read_raw().ok_or_else(bad)?;
        curve = Some(ec_params(&Some((raw[0], raw.to_vec()))).ok_or_else(bad)?);
    }
    let point = match r.optional(0xa1) {
        Some(c) => Some(Reader::new(c).bit_string().ok_or_else(bad)?.to_vec()),
        None => None,
    };
    let (curve, explicit) = curve.ok_or_else(bad)?;
    if d.len() > curve.field_len() {
        return Err(invalid_private());
    }
    let d = asn1::pad_be(&d, curve.field_len());
    let derived = curve.public_from_scalar(&d)?;
    let point = match point {
        Some(p) => {
            let p = curve.normalize_point(&p)?;
            if p != derived {
                return Err(invalid_private());
            }
            p
        }
        None => derived,
    };
    Ok(EcKey {
        curve,
        point,
        d: Some(d),
        explicit,
    })
}

/// Parses a legacy OpenSSL `DSAPrivateKey` (`SEQUENCE { 0, p, q, g, y, x }`).
pub fn parse_dsa_legacy(der: &[u8]) -> Option<DsaKey> {
    let (tag, body) = asn1::single(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    if r.small_uint()? != 0 {
        return None;
    }
    let p = r.biguint()?;
    let q = r.biguint()?;
    let g = r.biguint()?;
    let y = r.biguint()?;
    let x = r.biguint()?;
    r.finish()?;
    Some(DsaKey {
        p,
        q,
        g,
        y,
        x: Some(x),
    })
}

impl DsaKey {
    /// DER of the legacy OpenSSL `DSAPrivateKey`.
    pub fn legacy_der(&self) -> Option<Vec<u8>> {
        let x = self.x.as_ref()?;
        Some(asn1::seq(&[
            &asn1::small_uint(0),
            &asn1::biguint(&self.p),
            &asn1::biguint(&self.q),
            &asn1::biguint(&self.g),
            &asn1::biguint(&self.y),
            &asn1::biguint(x),
        ]))
    }
}

fn parse_spki(der: &[u8]) -> Option<KResult<AsymKey>> {
    let (tag, body) = asn1::single(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    let (oid, params) = parse_alg(&mut r)?;
    let key = r.bit_string()?;
    r.finish()?;
    if let Some(pss) = rsa_variant(&oid, &params) {
        let (n, e) = parse_pkcs1_public(key)?;
        return Some(Ok(AsymKey::Rsa(RsaKey {
            n,
            e,
            private: None,
            pss,
        })));
    }
    if oid == asn1::OID_DSA {
        let (p, q, g) = three_ints(&params?.1)?;
        let mut kr = Reader::new(key);
        let y = kr.biguint()?;
        kr.finish()?;
        return Some(Ok(AsymKey::Dsa(DsaKey {
            p,
            q,
            g,
            y,
            x: None,
        })));
    }
    if oid == asn1::OID_EC {
        let (curve, explicit) = ec_params(&params)?;
        return Some(curve.normalize_point(key).map(|point| {
            AsymKey::Ec(EcKey {
                curve,
                point,
                d: None,
                explicit,
            })
        }));
    }
    if let Some((ctor, kind, len)) = okp_ctor(&oid) {
        if params.is_some() || key.len() != len {
            return None;
        }
        if kind == "ed25519" || kind == "ed448" {
            let ok = if kind == "ed25519" {
                ed25519_dalek::VerifyingKey::from_bytes(key.try_into().ok()?).is_ok()
            } else {
                let arr: [u8; 57] = key.try_into().ok()?;
                ed448_goldilocks_plus::VerifyingKey::from_bytes(&arr).is_ok()
            };
            if !ok {
                return None;
            }
        }
        return Some(Ok(ctor(OkpKey {
            public: key.to_vec(),
            private: None,
        })));
    }
    if oid == asn1::OID_DH || oid == asn1::OID_DHX {
        let (p, g, q) = dh_params(&oid, &params)?;
        let y = Reader::new(key).biguint()?;
        return Some(Ok(AsymKey::Dh(DhKey {
            p,
            g,
            q,
            y,
            x: None,
        })));
    }
    None
}

fn parse_pkcs8(der: &[u8]) -> Option<KResult<AsymKey>> {
    let (tag, body) = asn1::single(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    let version = r.small_uint()?;
    if version > 1 {
        return None;
    }
    let (oid, params) = parse_alg(&mut r)?;
    let key = r.expect(TAG_OCTET_STRING)?;
    let _attributes = r.optional(0xa0);
    let _public = r.optional(0x81).or_else(|| r.optional(0xa1));
    r.finish()?;
    if let Some(pss) = rsa_variant(&oid, &params) {
        return Some(Ok(AsymKey::Rsa(parse_pkcs1_private(key, pss)?)));
    }
    if oid == asn1::OID_DSA {
        let (p, q, g) = three_ints(&params?.1)?;
        let x = Reader::new(key).biguint()?;
        let y = exp_public(&p, &g, &x)?;
        return Some(Ok(AsymKey::Dsa(DsaKey {
            p,
            q,
            g,
            y,
            x: Some(x),
        })));
    }
    if oid == asn1::OID_EC {
        let outer = ec_params(&params)?;
        return Some(parse_sec1(key, Some(outer)).map(AsymKey::Ec));
    }
    if let Some((ctor, kind, len)) = okp_ctor(&oid) {
        if params.is_some() {
            return None;
        }
        let (TAG_OCTET_STRING, raw) = asn1::single(key)? else {
            return None;
        };
        if raw.len() != len {
            return None;
        }
        return Some(okp_public(kind, raw).map(|public| {
            ctor(OkpKey {
                public,
                private: Some(raw.to_vec()),
            })
        }));
    }
    if oid == asn1::OID_DH || oid == asn1::OID_DHX {
        let (p, g, q) = dh_params(&oid, &params)?;
        let x = Reader::new(key).biguint()?;
        let y = exp_public(&p, &g, &x)?;
        return Some(Ok(AsymKey::Dh(DhKey {
            p,
            g,
            q,
            y,
            x: Some(x),
        })));
    }
    None
}
