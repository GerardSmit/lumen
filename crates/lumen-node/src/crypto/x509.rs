//! X.509 certificates (`crypto.X509Certificate`, the legacy peer-certificate object) and SPKAC
//! (`crypto.Certificate`). Certificate reading, the name and time text formats and PEM framing are
//! shared (`lumen_common::x509`); signatures and key checks use the RustCrypto crates.

use lumen::embed::OpError;
use lumen_common::codec::{self, Padding};
use lumen_common::pem::{self, PemError};
use lumen_common::x509::*;

use crate::hash::{self, Algo};

#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
    use super::*;

    use codec::hex_encode_upper as hex_upper;

    // ---- keys ---------------------------------------------------------------------------------------

    struct Spki<'a> {
        alg: String,
        params: Option<Tlv<'a>>,
        key: &'a [u8],
    }

    fn parse_spki(raw: &[u8]) -> Option<Spki<'_>> {
        let mut input = raw;
        let seq = expect(&mut input, SEQ)?;
        let mut s = seq.value;
        let alg = expect(&mut s, SEQ)?;
        let bits = expect(&mut s, BIT_STRING)?;
        let mut a = alg.value;
        let oid = expect(&mut a, OID)?;
        let params = read_tlv(&mut a);
        Some(Spki {
            alg: oid_text(oid.value),
            params,
            key: bits.value.get(1..)?,
        })
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Curve {
        P256,
        P384,
        P521,
        K256,
    }

    fn curve_of(params: Option<Tlv>) -> Option<Curve> {
        let p = params?;
        if p.tag != OID {
            return None;
        }
        Some(match oid_text(p.value).as_str() {
            "1.2.840.10045.3.1.7" => Curve::P256,
            "1.3.132.0.34" => Curve::P384,
            "1.3.132.0.35" => Curve::P521,
            "1.3.132.0.10" => Curve::K256,
            _ => return None,
        })
    }

    /// The uncompressed SEC1 encoding of a point on `curve`, or `None` when it is not one.
    fn normalize_point(curve: Curve, point: &[u8]) -> Option<Vec<u8>> {
        use elliptic_curve::sec1::ToEncodedPoint;
        Some(match curve {
            Curve::P256 => p256::PublicKey::from_sec1_bytes(point)
                .ok()?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::PublicKey::from_sec1_bytes(point)
                .ok()?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P521 => p521::PublicKey::from_sec1_bytes(point)
                .ok()?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::K256 => k256::PublicKey::from_sec1_bytes(point)
                .ok()?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
        })
    }

    fn rsa_public(key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        use pkcs1::der::Decode;
        let k = pkcs1::RsaPublicKey::from_der(key).ok()?;
        Some((
            k.modulus.as_bytes().to_vec(),
            k.public_exponent.as_bytes().to_vec(),
        ))
    }

    fn strip_zeros(b: &[u8]) -> &[u8] {
        let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
        &b[i..]
    }

    /// Whether OpenSSL could build an EVP_PKEY from this SPKI.
    fn spki_is_valid(spki: &Spki) -> bool {
        match spki.alg.as_str() {
            OID_RSA | OID_RSA_PSS => rsa_public(spki.key).is_some(),
            OID_EC => match curve_of(spki.params) {
                Some(curve) => normalize_point(curve, spki.key).is_some(),
                None => true,
            },
            OID_ED25519 | OID_X25519 => spki.key.len() == 32,
            OID_ED448 => spki.key.len() == 57,
            OID_X448 => spki.key.len() == 56,
            _ => true,
        }
    }

    /// The public half of a PKCS#8 private key as (algorithm OID, comparable public key bytes).
    fn private_to_public(pkcs8: &[u8]) -> Option<(String, Vec<u8>)> {
        let mut input = pkcs8;
        let seq = expect(&mut input, SEQ)?;
        let mut s = seq.value;
        expect(&mut s, INTEGER)?;
        let alg = expect(&mut s, SEQ)?;
        let key = expect(&mut s, OCTET_STRING)?;
        let mut a = alg.value;
        let oid = oid_text(expect(&mut a, OID)?.value);
        let params = read_tlv(&mut a);
        let public = match oid.as_str() {
            OID_RSA | OID_RSA_PSS => {
                use pkcs1::der::Decode;
                let k = pkcs1::RsaPrivateKey::from_der(key.value).ok()?;
                let mut v = strip_zeros(k.modulus.as_bytes()).to_vec();
                v.push(0xff);
                v.extend_from_slice(strip_zeros(k.public_exponent.as_bytes()));
                v
            }
            OID_EC => {
                use sec1::der::Decode;
                let k = sec1::EcPrivateKey::from_der(key.value).ok()?;
                let curve = curve_of(params).or_else(|| {
                    let oid = k.parameters?.named_curve()?;
                    curve_of_oid(&oid.to_string())
                })?;
                ec_public_from_secret(curve, k.private_key)?
            }
            OID_ED25519 => {
                let mut inner = key.value;
                let secret: [u8; 32] = expect(&mut inner, OCTET_STRING)?.value.try_into().ok()?;
                ed25519_dalek::SigningKey::from_bytes(&secret)
                    .verifying_key()
                    .to_bytes()
                    .to_vec()
            }
            OID_X25519 => {
                let mut inner = key.value;
                let secret: [u8; 32] = expect(&mut inner, OCTET_STRING)?.value.try_into().ok()?;
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret))
                    .as_bytes()
                    .to_vec()
            }
            _ => return None,
        };
        Some((oid, public))
    }

    fn curve_of_oid(oid: &str) -> Option<Curve> {
        Some(match oid {
            "1.2.840.10045.3.1.7" => Curve::P256,
            "1.3.132.0.34" => Curve::P384,
            "1.3.132.0.35" => Curve::P521,
            "1.3.132.0.10" => Curve::K256,
            _ => return None,
        })
    }

    fn ec_public_from_secret(curve: Curve, scalar: &[u8]) -> Option<Vec<u8>> {
        use elliptic_curve::sec1::ToEncodedPoint;
        Some(match curve {
            Curve::P256 => p256::SecretKey::from_slice(scalar)
                .ok()?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::SecretKey::from_slice(scalar)
                .ok()?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P521 => p521::SecretKey::from_slice(scalar)
                .ok()?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::K256 => k256::SecretKey::from_slice(scalar)
                .ok()?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
        })
    }

    /// The SPKI's public key in the comparable form `private_to_public` produces.
    fn spki_public(spki: &Spki) -> Option<Vec<u8>> {
        Some(match spki.alg.as_str() {
            OID_RSA | OID_RSA_PSS => {
                let (n, e) = rsa_public(spki.key)?;
                let mut v = strip_zeros(&n).to_vec();
                v.push(0xff);
                v.extend_from_slice(strip_zeros(&e));
                v
            }
            OID_EC => normalize_point(curve_of(spki.params)?, spki.key)?,
            _ => spki.key.to_vec(),
        })
    }

    // ---- signatures ---------------------------------------------------------------------------------

    fn hash_of_oid(oid: &str) -> Option<Algo> {
        Some(match oid {
            "1.2.840.113549.2.5" => Algo::Md5,
            "1.3.14.3.2.26" => Algo::Sha1,
            "2.16.840.1.101.3.4.2.4" => Algo::Sha224,
            "2.16.840.1.101.3.4.2.1" => Algo::Sha256,
            "2.16.840.1.101.3.4.2.2" => Algo::Sha384,
            "2.16.840.1.101.3.4.2.3" => Algo::Sha512,
            _ => return None,
        })
    }

    enum SigScheme {
        Pkcs1(Algo),
        Pss(Algo, usize),
        Ecdsa(Algo),
        Ed25519,
    }

    /// RSASSA-PSS-params: hash (default SHA-1), MGF1 over the same hash, salt length (default 20).
    fn pss_params(params: Option<Tlv>) -> Option<(Algo, usize)> {
        let mut hash = Algo::Sha1;
        let mut mgf_hash = Algo::Sha1;
        let mut salt = 20usize;
        if let Some(p) = params.filter(|p| p.tag == SEQ) {
            for field in children(p.value)? {
                let mut inner = field.value;
                match field.tag {
                    0xa0 => {
                        let mut a = expect(&mut inner, SEQ)?.value;
                        hash = hash_of_oid(&oid_text(expect(&mut a, OID)?.value))?;
                    }
                    0xa1 => {
                        let mut a = expect(&mut inner, SEQ)?.value;
                        if oid_text(expect(&mut a, OID)?.value) != "1.2.840.113549.1.1.8" {
                            return None;
                        }
                        let mut h = expect(&mut a, SEQ)?.value;
                        mgf_hash = hash_of_oid(&oid_text(expect(&mut h, OID)?.value))?;
                    }
                    0xa2 => {
                        let v = expect(&mut inner, INTEGER)?.value;
                        salt = v.iter().fold(0usize, |acc, &b| {
                            acc.saturating_mul(256).saturating_add(b as usize)
                        });
                    }
                    _ => {}
                }
            }
        }
        (hash == mgf_hash).then_some((hash, salt))
    }

    fn sig_scheme(alg: &Tlv) -> Option<SigScheme> {
        let mut a = alg.value;
        let oid = oid_text(expect(&mut a, OID)?.value);
        let params = read_tlv(&mut a);
        Some(match oid.as_str() {
            "1.2.840.113549.1.1.4" => SigScheme::Pkcs1(Algo::Md5),
            "1.2.840.113549.1.1.5" | "1.3.14.3.2.29" => SigScheme::Pkcs1(Algo::Sha1),
            "1.2.840.113549.1.1.14" => SigScheme::Pkcs1(Algo::Sha224),
            "1.2.840.113549.1.1.11" => SigScheme::Pkcs1(Algo::Sha256),
            "1.2.840.113549.1.1.12" => SigScheme::Pkcs1(Algo::Sha384),
            "1.2.840.113549.1.1.13" => SigScheme::Pkcs1(Algo::Sha512),
            OID_RSA_PSS => {
                let (h, salt) = pss_params(params)?;
                SigScheme::Pss(h, salt)
            }
            "1.2.840.10045.4.1" => SigScheme::Ecdsa(Algo::Sha1),
            "1.2.840.10045.4.3.1" => SigScheme::Ecdsa(Algo::Sha224),
            "1.2.840.10045.4.3.2" => SigScheme::Ecdsa(Algo::Sha256),
            "1.2.840.10045.4.3.3" => SigScheme::Ecdsa(Algo::Sha384),
            "1.2.840.10045.4.3.4" => SigScheme::Ecdsa(Algo::Sha512),
            OID_ED25519 => SigScheme::Ed25519,
            _ => return None,
        })
    }

    fn ecdsa_verify<C>(point: &[u8], hash: &[u8], sig: &[u8]) -> bool
    where
        C: elliptic_curve::PrimeCurve + elliptic_curve::CurveArithmetic,
        ecdsa::der::MaxSize<C>: elliptic_curve::generic_array::ArrayLength<u8>,
        <elliptic_curve::FieldBytesSize<C> as std::ops::Add>::Output:
            std::ops::Add<ecdsa::der::MaxOverhead> + elliptic_curve::generic_array::ArrayLength<u8>,
        elliptic_curve::AffinePoint<C>: ecdsa::hazmat::VerifyPrimitive<C>
            + elliptic_curve::sec1::FromEncodedPoint<C>
            + elliptic_curve::sec1::ToEncodedPoint<C>,
        elliptic_curve::FieldBytesSize<C>: elliptic_curve::sec1::ModulusSize,
    {
        use signature::hazmat::PrehashVerifier;
        let Ok(vk) = ecdsa::VerifyingKey::<C>::from_sec1_bytes(point) else {
            return false;
        };
        let Ok(sig) = ecdsa::Signature::<C>::from_der(sig) else {
            return false;
        };
        vk.verify_prehash(hash, &sig).is_ok()
    }

    /// Verify `signature` over `data` with the SPKI's key under the AlgorithmIdentifier `alg`.
    fn verify_signature(alg: &Tlv, spki_raw: &[u8], data: &[u8], signature: &[u8]) -> bool {
        let (Some(scheme), Some(spki)) = (sig_scheme(alg), parse_spki(spki_raw)) else {
            return false;
        };
        match scheme {
            SigScheme::Pkcs1(h) | SigScheme::Pss(h, _) => {
                let rsa_ok = matches!(scheme, SigScheme::Pkcs1(_)) && spki.alg == OID_RSA
                    || matches!(scheme, SigScheme::Pss(..))
                        && (spki.alg == OID_RSA || spki.alg == OID_RSA_PSS);
                if !rsa_ok {
                    return false;
                }
                let Some((n, e)) = rsa_public(spki.key) else {
                    return false;
                };
                let rsa_scheme = match scheme {
                    SigScheme::Pss(_, salt) => lumen_crypto::RsaScheme::Pss {
                        hash: h,
                        mgf1: h,
                        salt: lumen_crypto::PssSalt::Length(salt as u32),
                    },
                    _ => lumen_crypto::RsaScheme::Pkcs1 { hash: h },
                };
                let key = lumen_crypto::RsaPublicKey { n, e };
                lumen_crypto::backend()
                    .rsa_verify(&key, &rsa_scheme, &hash::digest(h, data), signature)
                    .unwrap_or(false)
            }
            SigScheme::Ecdsa(h) => {
                if spki.alg != OID_EC {
                    return false;
                }
                let hashed = hash::digest(h, data);
                match curve_of(spki.params) {
                    Some(Curve::P256) => {
                        ecdsa_verify::<p256::NistP256>(spki.key, &hashed, signature)
                    }
                    Some(Curve::P384) => {
                        ecdsa_verify::<p384::NistP384>(spki.key, &hashed, signature)
                    }
                    Some(Curve::P521) => {
                        ecdsa_verify::<p521::NistP521>(spki.key, &hashed, signature)
                    }
                    Some(Curve::K256) => {
                        ecdsa_verify::<k256::Secp256k1>(spki.key, &hashed, signature)
                    }
                    None => false,
                }
            }
            SigScheme::Ed25519 => {
                if spki.alg != OID_ED25519 {
                    return false;
                }
                let (Ok(key), Ok(sig)) = (
                    <[u8; 32]>::try_from(spki.key),
                    ed25519_dalek::Signature::from_slice(signature),
                ) else {
                    return false;
                };
                ed25519_dalek::VerifyingKey::from_bytes(&key)
                    .is_ok_and(|vk| vk.verify_strict(data, &sig).is_ok())
            }
        }
    }

    // ---- extensions ---------------------------------------------------------------------------------

    /// keyUsage bits, `None` when the extension is absent.
    fn key_usage(cert: &Cert) -> Option<u16> {
        let ext = cert.ext(OID_KEY_USAGE)?;
        let mut input = ext;
        let bits = expect(&mut input, BIT_STRING)?.value;
        let b0 = *bits.get(1).unwrap_or(&0) as u16;
        let b1 = *bits.get(2).unwrap_or(&0) as u16;
        Some(b0 | (b1 << 8))
    }

    const KU_KEY_CERT_SIGN: u16 = 0x04;

    /// `X509_check_ca(cert) == 1`.
    fn is_ca(cert: &Cert) -> bool {
        if key_usage(cert).is_some_and(|ku| ku & KU_KEY_CERT_SIGN == 0) {
            return false;
        }
        let Some(ext) = cert.ext(OID_BASIC_CONSTRAINTS) else {
            return false;
        };
        let mut input = ext;
        let Some(seq) = expect(&mut input, SEQ) else {
            return false;
        };
        let mut s = seq.value;
        matches!(read_tlv(&mut s), Some(t) if t.tag == BOOLEAN && t.value.first().is_some_and(|&b| b != 0))
    }

    fn subject_key_id<'a>(cert: &Cert<'a>) -> Option<&'a [u8]> {
        let mut input = cert.ext(OID_SKI)?;
        Some(expect(&mut input, OCTET_STRING)?.value)
    }

    /// `X509_check_issued(issuer, subject) == X509_V_OK`.
    fn check_issued(issuer: &Cert, subject: &Cert) -> bool {
        match (
            canonical_name(issuer.subject.value),
            canonical_name(subject.issuer.value),
        ) {
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
        if let Some(aki) = subject.ext(OID_AKI) {
            let mut input = aki;
            let Some(seq) = expect(&mut input, SEQ) else {
                return false;
            };
            let Some(fields) = children(seq.value) else {
                return false;
            };
            for field in fields {
                match field.tag {
                    0x80 => {
                        if subject_key_id(issuer).is_some_and(|ski| ski != field.value) {
                            return false;
                        }
                    }
                    0x82 => {
                        if strip_zeros(field.value) != strip_zeros(issuer.serial) {
                            return false;
                        }
                    }
                    _ => {}
                }
            }
        }
        !key_usage(issuer).is_some_and(|ku| ku & KU_KEY_CERT_SIGN == 0)
    }

    // ---- host / email / IP checks -------------------------------------------------------------------

    const FLAG_ALWAYS_CHECK_SUBJECT: u32 = 1;
    const FLAG_NO_WILDCARDS: u32 = 2;
    const FLAG_NO_PARTIAL_WILDCARDS: u32 = 4;
    const FLAG_MULTI_LABEL_WILDCARDS: u32 = 8;
    const FLAG_SINGLE_LABEL_SUBDOMAINS: u32 = 16;
    const FLAG_NEVER_CHECK_SUBJECT: u32 = 32;
    const FLAG_DOT_SUBDOMAINS: u32 = 0x8000;

    fn skip_prefix<'p>(pattern: &'p [u8], subject_len: usize, flags: u32) -> &'p [u8] {
        if flags & FLAG_DOT_SUBDOMAINS == 0 {
            return pattern;
        }
        let mut p = pattern;
        while p.len() > subject_len && p[0] != 0 {
            if flags & FLAG_SINGLE_LABEL_SUBDOMAINS != 0 && p[0] == b'.' {
                break;
            }
            p = &p[1..];
        }
        if p.len() == subject_len {
            p
        } else {
            pattern
        }
    }

    fn equal_nocase(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
        let pattern = skip_prefix(pattern, subject.len(), flags);
        pattern.len() == subject.len()
            && pattern
                .iter()
                .zip(subject)
                .all(|(&l, &r)| l != 0 && l.to_ascii_lowercase() == r.to_ascii_lowercase())
    }

    fn equal_case(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
        let pattern = skip_prefix(pattern, subject.len(), flags);
        pattern == subject
    }

    fn equal_email(a: &[u8], b: &[u8], _flags: u32) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut i = a.len();
        while i > 0 {
            i -= 1;
            if a[i] == b'@' || b[i] == b'@' {
                if !equal_nocase(&a[i..], &b[i..], 0) {
                    return false;
                }
                break;
            }
        }
        if i == 0 {
            i = a.len();
        }
        equal_case(&a[..i], &b[..i], 0)
    }

    fn valid_star(p: &[u8], flags: u32) -> Option<usize> {
        const START: u8 = 1;
        const IDNA: u8 = 2;
        const HYPHEN: u8 = 4;
        let mut star = None;
        let mut state = START;
        let mut dots = 0;
        for i in 0..p.len() {
            let c = p[i];
            if c == b'*' {
                let at_start = state & START != 0;
                let at_end = i == p.len() - 1 || p[i + 1] == b'.';
                if star.is_some() || state & IDNA != 0 || dots > 0 {
                    return None;
                }
                if flags & FLAG_NO_PARTIAL_WILDCARDS != 0 && (!at_start || !at_end) {
                    return None;
                }
                if !at_start && !at_end {
                    return None;
                }
                star = Some(i);
                state &= !START;
            } else if c.is_ascii_alphanumeric() {
                if state & START != 0
                    && p.len() - i >= 4
                    && p[i..i + 4].eq_ignore_ascii_case(b"xn--")
                {
                    state |= IDNA;
                }
                state &= !(HYPHEN | START);
            } else if c == b'.' {
                if state & (HYPHEN | START) != 0 {
                    return None;
                }
                state = START;
                dots += 1;
            } else if c == b'-' {
                if state & START != 0 {
                    return None;
                }
                state |= HYPHEN;
            } else {
                return None;
            }
        }
        if state & (START | HYPHEN) != 0 || dots < 2 {
            return None;
        }
        star
    }

    fn wildcard_match(prefix: &[u8], suffix: &[u8], subject: &[u8], flags: u32) -> bool {
        if subject.len() < prefix.len() + suffix.len() {
            return false;
        }
        if !equal_nocase(prefix, &subject[..prefix.len()], flags) {
            return false;
        }
        let ws = prefix.len();
        let we = subject.len() - suffix.len();
        if !equal_nocase(&subject[we..], suffix, flags) {
            return false;
        }
        let mut allow_multi = false;
        let mut allow_idna = false;
        if prefix.is_empty() && suffix.first() == Some(&b'.') {
            if ws == we {
                return false;
            }
            allow_idna = true;
            allow_multi = flags & FLAG_MULTI_LABEL_WILDCARDS != 0;
        }
        if !allow_idna && subject.len() >= 4 && subject[..4].eq_ignore_ascii_case(b"xn--") {
            return false;
        }
        if we == ws + 1 && subject[ws] == b'*' {
            return true;
        }
        subject[ws..we]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || (allow_multi && c == b'.'))
    }

    fn equal_wildcard(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
        let star = if subject.len() > 1 && subject[0] == b'.' {
            None
        } else {
            valid_star(pattern, flags)
        };
        match star {
            None => equal_nocase(pattern, subject, flags),
            Some(i) => wildcard_match(&pattern[..i], &pattern[i + 1..], subject, flags),
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum CheckType {
        Email,
        Dns,
        Ip,
    }

    /// `do_x509_check`: the matched name, `Ok(None)` for no match.
    fn do_check(
        cert: &Cert,
        chk: &[u8],
        mut flags: u32,
        kind: CheckType,
    ) -> Result<Option<Vec<u8>>, ()> {
        let (gen_tag, cn_oid): (u8, Option<&str>) = match kind {
            CheckType::Email => (0x81, Some("1.2.840.113549.1.9.1")),
            CheckType::Dns => {
                if chk.len() > 1 && chk[0] == b'.' {
                    flags |= FLAG_DOT_SUBDOMAINS;
                }
                (0x82, Some("2.5.4.3"))
            }
            CheckType::Ip => (0x87, None),
        };
        let equal = |pattern: &[u8], subject: &[u8]| -> bool {
            match kind {
                CheckType::Email => equal_email(pattern, subject, flags),
                CheckType::Dns if flags & FLAG_NO_WILDCARDS != 0 => {
                    equal_nocase(pattern, subject, flags)
                }
                CheckType::Dns => equal_wildcard(pattern, subject, flags),
                CheckType::Ip => equal_case(pattern, subject, flags),
            }
        };
        if let Some(ext) = cert.ext(OID_SAN) {
            if let Some(names) = general_names(ext) {
                let mut san_present = false;
                for gen in names.iter().filter(|g| g.tag == gen_tag) {
                    san_present = true;
                    if !gen.value.is_empty() && equal(gen.value, chk) {
                        return Ok(Some(gen.value.to_vec()));
                    }
                }
                if san_present && flags & FLAG_ALWAYS_CHECK_SUBJECT == 0 {
                    return Ok(None);
                }
            }
        }
        let Some(cn_oid) = cn_oid else {
            return Ok(None);
        };
        if flags & FLAG_NEVER_CHECK_SUBJECT != 0 {
            return Ok(None);
        }
        for rdn in name_rdns(cert.subject.value).unwrap_or_default() {
            for entry in rdn.iter().filter(|e| e.oid == cn_oid) {
                if entry.value.value.is_empty() {
                    continue;
                }
                let Some(s) = string_to_utf8(entry.value.tag, entry.value.value) else {
                    return Err(());
                };
                if equal(s.as_bytes(), chk) {
                    return Ok(Some(s.into_bytes()));
                }
            }
        }
        Ok(None)
    }

    fn invalid_arg(message: &'static str) -> OpError {
        OpError::type_error(message).with_code("ERR_INVALID_ARG_VALUE")
    }

    fn operation_failed() -> OpError {
        OpError::error("Operation failed").with_code("ERR_CRYPTO_OPERATION_FAILED")
    }

    /// The name argument of `X509_check_host` / `X509_check_email`: NUL only as the final byte.
    fn check_name(name: &str) -> Result<&[u8], OpError> {
        let b = name.as_bytes();
        let scan = if b.len() > 1 { &b[..b.len() - 1] } else { b };
        if scan.contains(&0) {
            return Err(invalid_arg("Invalid name"));
        }
        Ok(if b.len() > 1 && b[b.len() - 1] == 0 {
            &b[..b.len() - 1]
        } else {
            b
        })
    }

    fn asn1_error() -> OpError {
        OpError::error("error:068000A8:asn1 encoding routines::wrong tag")
            .with_code("ERR_OSSL_ASN1_WRONG_TAG")
    }

    fn cert_of(der: &[u8]) -> Result<Cert<'_>, OpError> {
        parse_cert(der).ok_or_else(asn1_error)
    }

    // ---- ops ----------------------------------------------------------------------------------------

    /// `parseX509`: the DER of a PEM (`CERTIFICATE`, `X509 CERTIFICATE`, `TRUSTED CERTIFICATE`) or DER
    /// certificate.
    #[op(name = "x509Parse")]
    fn x509_parse(input: &[u8]) -> Result<Vec<u8>, OpError> {
        match pem::find(
            input,
            &["CERTIFICATE", "X509 CERTIFICATE", "TRUSTED CERTIFICATE"],
        ) {
            Ok(der) => parse_cert(&der)
                .map(|c| c.raw.to_vec())
                .ok_or_else(asn1_error),
            Err(PemError::BadBase64) => Err(OpError::error(
                "error:04800064:PEM routines::bad base64 decode",
            )
            .with_code("ERR_OSSL_PEM_BAD_BASE64_DECODE")),
            Err(PemError::NoStartLine) => {
                parse_cert(input).map(|c| c.raw.to_vec()).ok_or_else(|| {
                    OpError::error("error:0480006C:PEM routines::no start line")
                        .with_code("ERR_OSSL_PEM_NO_START_LINE")
                })
            }
        }
    }

    #[op(name = "x509Subject")]
    fn x509_subject(der: &[u8]) -> Result<Option<String>, OpError> {
        let c = cert_of(der)?;
        Ok(print_name(c.subject.value, false).map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    #[op(name = "x509Issuer")]
    fn x509_issuer(der: &[u8]) -> Result<Option<String>, OpError> {
        let c = cert_of(der)?;
        Ok(print_name(c.issuer.value, false).map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// `[present, text]`: text is `null` when the extension cannot be decoded.
    #[op(name = "x509SubjectAltName")]
    fn x509_subject_alt_name(der: &[u8]) -> Result<(bool, Option<String>), OpError> {
        let c = cert_of(der)?;
        Ok(match c.ext(OID_SAN) {
            Some(ext) => (true, san_text(ext)),
            None => (false, None),
        })
    }

    #[op(name = "x509InfoAccess")]
    fn x509_info_access(der: &[u8]) -> Result<(bool, Option<String>), OpError> {
        let c = cert_of(der)?;
        Ok(match c.ext(OID_AIA) {
            Some(ext) => (true, info_access_text(ext)),
            None => (false, None),
        })
    }

    /// `[validFrom, validTo]`.
    #[op(name = "x509Validity")]
    fn x509_validity(der: &[u8]) -> Result<(String, String), OpError> {
        let c = cert_of(der)?;
        Ok((
            print_time(&c.not_before).unwrap_or_default(),
            print_time(&c.not_after).unwrap_or_default(),
        ))
    }

    #[op(name = "x509Fingerprint")]
    fn x509_fingerprint(der: &[u8], algorithm: &str) -> Result<String, OpError> {
        let c = cert_of(der)?;
        let algo = Algo::from_name(algorithm)
            .ok_or_else(|| OpError::error("Digest method not supported"))?;
        let md = hash::digest(algo, c.raw);
        Ok(md
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"))
    }

    /// The extended key usage OIDs, `null` without the extension.
    #[op(name = "x509KeyUsage")]
    fn x509_key_usage(der: &[u8]) -> Result<Option<Vec<String>>, OpError> {
        let c = cert_of(der)?;
        let Some(ext) = c.ext(OID_EKU) else {
            return Ok(None);
        };
        let mut input = ext;
        let Some(seq) = expect(&mut input, SEQ) else {
            return Ok(None);
        };
        Ok(children(seq.value).map(|oids| {
            oids.iter()
                .filter(|t| t.tag == OID)
                .map(|t| oid_text(t.value))
                .collect()
        }))
    }

    /// `BN_bn2hex` of the serial number.
    #[op(name = "x509SerialNumber")]
    fn x509_serial_number(der: &[u8]) -> Result<String, OpError> {
        let c = cert_of(der)?;
        let negative = c.serial.first().is_some_and(|&b| b & 0x80 != 0);
        let magnitude = if negative {
            let mut v = c.serial.to_vec();
            let mut carry = true;
            for b in v.iter_mut().rev() {
                *b = !*b;
                if carry {
                    let (r, o) = b.overflowing_add(1);
                    *b = r;
                    carry = o;
                }
            }
            v
        } else {
            c.serial.to_vec()
        };
        let hex = hex_upper(strip_zeros(&magnitude));
        Ok(match (negative, hex.is_empty()) {
            (_, true) => "0".to_string(),
            (true, false) => format!("-{hex}"),
            (false, false) => hex,
        })
    }

    #[op(name = "x509Pem")]
    fn x509_pem(der: &[u8]) -> Result<String, OpError> {
        let c = cert_of(der)?;
        Ok(pem::encode("CERTIFICATE", &[], c.raw))
    }

    /// The SubjectPublicKeyInfo DER; throws like `X509_get_pubkey` when the key cannot be decoded.
    #[op(name = "x509PublicKey")]
    fn x509_public_key(der: &[u8]) -> Result<Vec<u8>, OpError> {
        let c = cert_of(der)?;
        match parse_spki(c.spki.raw) {
            Some(spki) if spki_is_valid(&spki) => Ok(c.spki.raw.to_vec()),
            _ => Err(
                OpError::error("error:03000072:digital envelope routines::decode error")
                    .with_code("ERR_OSSL_EVP_DECODE_ERROR"),
            ),
        }
    }

    #[op(name = "x509CheckCA")]
    fn x509_check_ca(der: &[u8]) -> Result<bool, OpError> {
        Ok(is_ca(&cert_of(der)?))
    }

    #[op(name = "x509CheckHost")]
    fn x509_check_host(der: &[u8], name: &str, flags: u32) -> Result<Option<String>, OpError> {
        let c = cert_of(der)?;
        let chk = check_name(name)?;
        match do_check(&c, chk, flags, CheckType::Dns) {
            Ok(found) => Ok(found.map(|b| b.iter().map(|&x| x as char).collect())),
            Err(()) => Err(operation_failed()),
        }
    }

    #[op(name = "x509CheckEmail")]
    fn x509_check_email(der: &[u8], email: &str, flags: u32) -> Result<bool, OpError> {
        let c = cert_of(der)?;
        let chk = check_name(email)?;
        match do_check(&c, chk, flags, CheckType::Email) {
            Ok(found) => Ok(found.is_some()),
            Err(()) => Err(operation_failed()),
        }
    }

    /// `X509_check_ip_asc`.
    #[op(name = "x509CheckIP")]
    fn x509_check_ip(der: &[u8], ip: &str, flags: u32) -> Result<bool, OpError> {
        let c = cert_of(der)?;
        let addr: Vec<u8> = match ip.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(a)) => a.octets().to_vec(),
            Ok(std::net::IpAddr::V6(a)) => a.octets().to_vec(),
            Err(_) => return Err(invalid_arg("Invalid IP")),
        };
        match do_check(&c, &addr, flags, CheckType::Ip) {
            Ok(found) => Ok(found.is_some()),
            Err(()) => Err(operation_failed()),
        }
    }

    /// Whether `issuer` issued `der` (`X509_check_issued`).
    #[op(name = "x509CheckIssued")]
    fn x509_check_issued(der: &[u8], issuer: &[u8]) -> Result<bool, OpError> {
        Ok(check_issued(&cert_of(issuer)?, &cert_of(der)?))
    }

    /// Whether the PKCS#8 private key matches the certificate's public key.
    #[op(name = "x509CheckPrivateKey")]
    fn x509_check_private_key(der: &[u8], pkcs8: &[u8]) -> Result<bool, OpError> {
        let c = cert_of(der)?;
        let Some(spki) = parse_spki(c.spki.raw) else {
            return Ok(false);
        };
        let Some((alg, public)) = private_to_public(pkcs8) else {
            return Ok(false);
        };
        Ok(alg == spki.alg && spki_public(&spki).is_some_and(|p| p == public))
    }

    /// Whether the certificate's signature verifies under the SPKI public key.
    #[op(name = "x509Verify")]
    fn x509_verify(der: &[u8], spki: &[u8]) -> Result<bool, OpError> {
        let c = cert_of(der)?;
        Ok(verify_signature(&c.sig_alg, spki, c.tbs, c.signature))
    }

    /// The entries of the subject (`issuer` false) or issuer name as `[type, value, ...]` for the
    /// legacy object; `null` when a value is not a string.
    #[op(name = "x509NameEntries")]
    fn x509_name_entries(der: &[u8], issuer: bool) -> Result<Option<Vec<String>>, OpError> {
        let c = cert_of(der)?;
        let name = if issuer { c.issuer } else { c.subject };
        let Some(rdns) = name_rdns(name.value) else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for entry in rdns.iter().flatten() {
            let Some(value) = string_to_utf8(entry.value.tag, entry.value.value) else {
                return Ok(None);
            };
            out.push(
                attribute_short_name(&entry.oid).map_or_else(|| entry.oid.clone(), str::to_string),
            );
            out.push(value);
        }
        Ok(Some(out))
    }

    /// Key details of the legacy object: `["rsa", [modulus, exponent], bits, pubkey]`,
    /// `["ec", [asn1Curve, nistCurve], bits, point]` or `["", [], 0, []]`.
    #[op(name = "x509KeyDetails")]
    fn x509_key_details(der: &[u8]) -> Result<(String, Vec<String>, u32, Vec<u8>), OpError> {
        let c = cert_of(der)?;
        let none = || (String::new(), Vec::new(), 0, Vec::new());
        let Some(spki) = parse_spki(c.spki.raw) else {
            return Ok(none());
        };
        match spki.alg.as_str() {
            OID_RSA => {
                let Some((n, e)) = rsa_public(spki.key) else {
                    return Ok(none());
                };
                let n = strip_zeros(&n);
                let e = strip_zeros(&e);
                let mut modulus = hex_upper(n).trim_start_matches('0').to_string();
                if modulus.is_empty() {
                    modulus.push('0');
                }
                let bits = n
                    .first()
                    .map_or(0, |&b| (n.len() as u32 - 1) * 8 + (8 - b.leading_zeros()));
                let exponent = format!(
                    "0x{:x}",
                    e.iter().take(8).fold(0u64, |acc, &b| (acc << 8) | b as u64)
                );
                Ok((
                    "rsa".into(),
                    vec![modulus, exponent],
                    bits,
                    c.spki.raw.to_vec(),
                ))
            }
            OID_EC => {
                let Some(curve) = curve_of(spki.params) else {
                    return Ok(none());
                };
                let (sn, nist, bits) = match curve {
                    Curve::P256 => ("prime256v1", "P-256", 256),
                    Curve::P384 => ("secp384r1", "P-384", 384),
                    Curve::P521 => ("secp521r1", "P-521", 521),
                    Curve::K256 => ("secp256k1", "", 256),
                };
                Ok((
                    "ec".into(),
                    vec![sn.into(), nist.into()],
                    bits,
                    spki.key.to_vec(),
                ))
            }
            _ => Ok(none()),
        }
    }

    // ---- SPKAC --------------------------------------------------------------------------------------

    /// The DER of a base64 SignedPublicKeyAndChallenge, trimmed like `EVP_DecodeBlock`.
    fn spkac_decode(input: &[u8]) -> Option<Vec<u8>> {
        let start = input
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(input.len());
        let mut end = input.len();
        while end > start + 3
            && !(input[end - 1].is_ascii_alphanumeric()
                || matches!(input[end - 1], b'+' | b'/' | b'='))
        {
            end -= 1;
        }
        let text = &input[start..end];
        codec::base64_decode_strict(text, false, Padding::Optional).ok()
    }

    struct Spkac<'a> {
        pkac: &'a [u8],
        spki: &'a [u8],
        challenge: Tlv<'a>,
        sig_alg: Tlv<'a>,
        signature: &'a [u8],
    }

    fn spkac_parse(der: &[u8]) -> Option<Spkac<'_>> {
        let mut input = der;
        let seq = expect(&mut input, SEQ)?;
        let mut s = seq.value;
        let pkac = expect(&mut s, SEQ)?;
        let sig_alg = expect(&mut s, SEQ)?;
        let sig = expect(&mut s, BIT_STRING)?;
        let mut p = pkac.value;
        let spki = expect(&mut p, SEQ)?;
        let challenge = read_tlv(&mut p)?;
        Some(Spkac {
            pkac: pkac.raw,
            spki: spki.raw,
            challenge,
            sig_alg,
            signature: sig.value.get(1..)?,
        })
    }

    fn spkac_too_large(input: &[u8]) -> Result<(), OpError> {
        if input.len() > i32::MAX as usize {
            return Err(OpError::range_error("spkac is too large").with_code("ERR_OUT_OF_RANGE"));
        }
        Ok(())
    }

    #[op(name = "certVerifySpkac")]
    fn cert_verify_spkac(input: &[u8]) -> Result<bool, OpError> {
        spkac_too_large(input)?;
        let Some(der) = spkac_decode(input) else {
            return Ok(false);
        };
        let Some(s) = spkac_parse(&der) else {
            return Ok(false);
        };
        match parse_spki(s.spki) {
            Some(spki) if spki_is_valid(&spki) => {
                Ok(verify_signature(&s.sig_alg, s.spki, s.pkac, s.signature))
            }
            _ => Ok(false),
        }
    }

    /// The SPKAC's public key as PEM, `null` when it cannot be decoded.
    #[op(name = "certExportPublicKey")]
    fn cert_export_public_key(input: &[u8]) -> Result<Option<Vec<u8>>, OpError> {
        spkac_too_large(input)?;
        let Some(der) = spkac_decode(input) else {
            return Ok(None);
        };
        let Some(s) = spkac_parse(&der) else {
            return Ok(None);
        };
        match parse_spki(s.spki) {
            Some(spki) if spki_is_valid(&spki) => {
                Ok(Some(pem::encode("PUBLIC KEY", &[], s.spki).into_bytes()))
            }
            _ => Ok(None),
        }
    }

    #[op(name = "certExportChallenge")]
    fn cert_export_challenge(input: &[u8]) -> Result<Option<Vec<u8>>, OpError> {
        spkac_too_large(input)?;
        let Some(der) = spkac_decode(input) else {
            return Ok(None);
        };
        let Some(s) = spkac_parse(&der) else {
            return Ok(None);
        };
        Ok(string_to_utf8(s.challenge.tag, s.challenge.value).map(String::into_bytes))
    }
}
