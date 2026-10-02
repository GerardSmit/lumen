//! The elliptic curves lumen supports (those with a RustCrypto arithmetic crate) and their names
//! and parameters as OpenSSL spells them.

use der::asn1::ObjectIdentifier;

use super::KResult;

/// A named prime curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EcCurve {
    P256,
    P384,
    P521,
    Secp256k1,
}

/// Runs `$body` with `$C` bound to the curve type of `$curve` (an [`EcCurve`]).
#[macro_export]
macro_rules! with_ec_curve {
    ($curve:expr, $C:ident => $body:expr) => {
        match $curve {
            $crate::crypto::keys::EcCurve::P256 => {
                type $C = p256::NistP256;
                $body
            }
            $crate::crypto::keys::EcCurve::P384 => {
                type $C = p384::NistP384;
                $body
            }
            $crate::crypto::keys::EcCurve::P521 => {
                type $C = p521::NistP521;
                $body
            }
            $crate::crypto::keys::EcCurve::Secp256k1 => {
                type $C = k256::Secp256k1;
                $body
            }
        }
    };
}
pub use with_ec_curve;

pub const ALL: [EcCurve; 4] = [EcCurve::P256, EcCurve::P384, EcCurve::P521, EcCurve::Secp256k1];

impl EcCurve {
    /// OpenSSL's short name (`keyDetail().namedCurve`, `getCurves()`).
    pub fn name(self) -> &'static str {
        match self {
            EcCurve::P256 => "prime256v1",
            EcCurve::P384 => "secp384r1",
            EcCurve::P521 => "secp521r1",
            EcCurve::Secp256k1 => "secp256k1",
        }
    }

    /// The JWK / WebCrypto name, for the curves JWK defines.
    pub fn jwk_name(self) -> Option<&'static str> {
        match self {
            EcCurve::P256 => Some("P-256"),
            EcCurve::P384 => Some("P-384"),
            EcCurve::P521 => Some("P-521"),
            EcCurve::Secp256k1 => Some("secp256k1"),
        }
    }

    /// A curve by any name OpenSSL accepts (short names, NIST names, aliases) or a JWK name.
    pub fn from_name(name: &str) -> Option<EcCurve> {
        Some(match name {
            "prime256v1" | "secp256r1" | "P-256" => EcCurve::P256,
            "secp384r1" | "P-384" => EcCurve::P384,
            "secp521r1" | "P-521" => EcCurve::P521,
            "secp256k1" => EcCurve::Secp256k1,
            _ => return None,
        })
    }

    pub fn oid(self) -> ObjectIdentifier {
        match self {
            EcCurve::P256 => ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7"),
            EcCurve::P384 => ObjectIdentifier::new_unwrap("1.3.132.0.34"),
            EcCurve::P521 => ObjectIdentifier::new_unwrap("1.3.132.0.35"),
            EcCurve::Secp256k1 => ObjectIdentifier::new_unwrap("1.3.132.0.10"),
        }
    }

    pub fn from_oid(oid: &ObjectIdentifier) -> Option<EcCurve> {
        ALL.into_iter().find(|c| c.oid() == *oid)
    }

    /// Byte length of a field element / scalar.
    pub fn field_len(self) -> usize {
        match self {
            EcCurve::P256 | EcCurve::Secp256k1 => 32,
            EcCurve::P384 => 48,
            EcCurve::P521 => 66,
        }
    }

    /// The explicit `ECParameters` DER, byte-identical to what OpenSSL writes for
    /// `paramEncoding: 'explicit'`.
    pub fn explicit_params(self) -> Vec<u8> {
        let hex = match self {
            EcCurve::P256 => "3081f7020101302c06072a8648ce3d0101022100ffffffff00000001000000000000000000000000ffffffffffffffffffffffff305b0420ffffffff00000001000000000000000000000000fffffffffffffffffffffffc04205ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b031500c49d360886e704936a6678e1139d26b7819f7e900441046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5022100ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551020101",
            EcCurve::P384 => "30820157020101303c06072a8648ce3d0101023100fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff307b0430fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000fffffffc0430b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef031500a335926aa319a27a1d00896a6773a4827acdac73046104aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab73617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f023100ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973020101",
            EcCurve::P521 => "308201c3020101304d06072a8648ce3d0101024201ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff30819f044201fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffc04420051953eb9618e1c9a1f929a21a0b68540eea2da725b99b315f3b8b489918ef109e156193951ec7e937b1652c0bd3bb1bf073573df883d2c34f1ef451fd46b503f00031500d09e8800291cb85396cc6717393284aaa0da64ba0481850400c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650024201fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409020101",
            EcCurve::Secp256k1 => "3081e0020101302c06072a8648ce3d0101022100fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f3044042000000000000000000000000000000000000000000000000000000000000000000420000000000000000000000000000000000000000000000000000000000000000704410479be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8022100fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141020101",
        };
        lumen_common::codec::hex_decode_lenient(hex.as_bytes())
    }

    /// The curve whose explicit parameters are `params` (an `ECParameters` element). The optional
    /// seed is ignored; the field, coefficients, generator and order must match a known curve.
    pub fn from_explicit(params: &[u8]) -> Option<EcCurve> {
        let want = explicit_core(params)?;
        ALL.into_iter().find(|c| explicit_core(&c.explicit_params()).as_ref() == Some(&want))
    }

    /// Validates `point` (SEC1, compressed or not) and returns it uncompressed.
    pub fn normalize_point(self, point: &[u8]) -> KResult<Vec<u8>> {
        use elliptic_curve::sec1::ToEncodedPoint;
        with_ec_curve!(self, C => {
            let pk = elliptic_curve::PublicKey::<C>::from_sec1_bytes(point).map_err(|_| super::invalid_point())?;
            Ok(pk.to_encoded_point(false).as_bytes().to_vec())
        })
    }

    /// The uncompressed public point of the scalar `d` (big-endian, any length up to the field).
    pub fn public_from_scalar(self, d: &[u8]) -> KResult<Vec<u8>> {
        use elliptic_curve::sec1::ToEncodedPoint;
        let padded = super::asn1::pad_be(d, self.field_len());
        with_ec_curve!(self, C => {
            let sk = elliptic_curve::SecretKey::<C>::from_slice(&padded).map_err(|_| super::invalid_private())?;
            Ok(sk.public_key().to_encoded_point(false).as_bytes().to_vec())
        })
    }

    /// A random private scalar (big-endian, field length) and its uncompressed public point.
    pub fn generate(self) -> (Vec<u8>, Vec<u8>) {
        use elliptic_curve::sec1::ToEncodedPoint;
        with_ec_curve!(self, C => {
            let sk = elliptic_curve::SecretKey::<C>::random(&mut lumen_crypto::SysRng);
            (sk.to_bytes().to_vec(), sk.public_key().to_encoded_point(false).as_bytes().to_vec())
        })
    }
}

fn explicit_core(params: &[u8]) -> Option<Vec<Vec<u8>>> {
    use super::asn1::*;
    let (tag, body) = single(params)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let mut r = Reader::new(body);
    r.small_uint()?;
    let mut field = r.sequence()?;
    if field.oid()? != OID_PRIME_FIELD {
        return None;
    }
    let p = field.uint()?.to_vec();
    let mut curve = r.sequence()?;
    let a = curve.expect(TAG_OCTET_STRING)?;
    let b = curve.expect(TAG_OCTET_STRING)?;
    let base = r.expect(TAG_OCTET_STRING)?;
    let order = r.uint()?.to_vec();
    let strip = |v: &[u8]| v[v.iter().position(|x| *x != 0).unwrap_or(v.len())..].to_vec();
    Some(vec![p, strip(a), strip(b), base.to_vec(), order])
}
