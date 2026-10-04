//! Key pair generation (`generateKeyPair`): RSA / RSA-PSS (`rsa`), DSA (`dsa`), EC (the curve
//! crates), Ed25519 / Ed448 / X25519 / X448, and finite-field DH over `num-bigint-dig`.

use num_bigint_dig::{prime::probably_prime, BigUint, RandBigInt, RandPrime};
use rand_core::OsRng;

use super::curves::EcCurve;
use super::model::{okp_public, AsymKey, DhKey, DsaKey, EcKey, OkpKey, PssParams, RsaKey};
use super::{KResult, SendError};

/// Miller-Rabin rounds for generated domain parameters.
const MR_ROUNDS: usize = 64;

fn exponent_error() -> SendError {
    SendError::new(
        "Error",
        "error:1C80006F:Provider routines::invalid public exponent",
    )
    .with_code("ERR_OSSL_PUB_EXPONENT_OUT_OF_RANGE")
}

/// An RSA key of `bits` bits with public exponent `e`; `pss` makes it an RSASSA-PSS key (with
/// parameter restrictions when given).
pub fn rsa(bits: u32, e: u32, pss: Option<Option<PssParams>>) -> KResult<AsymKey> {
    if e < 3 || e % 2 == 0 {
        return Err(exponent_error());
    }
    if bits < 512 {
        return Err(SendError::new(
            "Error",
            "error:1C80006B:Provider routines::key size too small",
        )
        .with_code("ERR_OSSL_KEY_SIZE_TOO_SMALL"));
    }
    let k = rsa::RsaPrivateKey::new_with_exp(&mut OsRng, bits as usize, &BigUint::from(e))
        .map_err(|err| SendError::new("Error", format!("RSA key generation failed: {err}")))?;
    Ok(AsymKey::Rsa(RsaKey::from_rsa_private(&k, pss)))
}

/// DSA domain parameters with a `l`-bit `p` and `n`-bit `q` (FIPS 186 A.1.1-style search, with
/// the `dsa` crate's generator for its standard sizes).
fn dsa_params(l: u32, n: u32) -> KResult<(BigUint, BigUint, BigUint)> {
    #[allow(deprecated)]
    let standard = match (l, n) {
        (1024, 160) => Some(dsa::KeySize::DSA_1024_160),
        (2048, 224) => Some(dsa::KeySize::DSA_2048_224),
        (2048, 256) => Some(dsa::KeySize::DSA_2048_256),
        (3072, 256) => Some(dsa::KeySize::DSA_3072_256),
        _ => None,
    };
    if let Some(size) = standard {
        let c = dsa::Components::generate(&mut OsRng, size);
        return Ok((c.p().clone(), c.q().clone(), c.g().clone()));
    }
    if n < 2 || n >= l || l < 512 {
        return Err(SendError::new(
            "Error",
            "error:1C800069:Provider routines::invalid key length",
        )
        .with_code("ERR_OSSL_INVALID_KEY_LENGTH"));
    }
    let one = BigUint::from(1u8);
    let two = BigUint::from(2u8);
    let p_min = BigUint::from(1u8) << (l as usize - 1);
    loop {
        let q = OsRng.gen_prime(n as usize);
        let two_q = &two * &q;
        for _ in 0..4 * l {
            let m = OsRng.gen_biguint(l as usize) | &p_min;
            let p = &m - (&m % &two_q) + &one;
            if p.bits() != l as usize || !probably_prime(&p, MR_ROUNDS) {
                continue;
            }
            let e = (&p - &one) / &q;
            let mut h = two.clone();
            let g = loop {
                let g = h.modpow(&e, &p);
                if g != one {
                    break g;
                }
                h += &one;
            };
            return Ok((p, q, g));
        }
    }
}

/// A DSA key with an `l`-bit prime; `divisor` is the bit length of `q` (OpenSSL's default when
/// `None`: 256 for `l >= 2048`, else 160).
pub fn dsa(l: u32, divisor: Option<u32>) -> KResult<AsymKey> {
    let n = divisor.unwrap_or(if l >= 2048 { 256 } else { 160 });
    let (p, q, g) = dsa_params(l, n)?;
    let components = dsa::Components::from_components(p.clone(), q.clone(), g.clone())
        .map_err(|_| SendError::new("Error", "DSA parameter generation failed"))?;
    let sk = dsa::SigningKey::generate(&mut OsRng, components);
    Ok(AsymKey::Dsa(DsaKey {
        p,
        q,
        g,
        y: sk.verifying_key().y().clone(),
        x: Some(sk.x().clone()),
    }))
}

pub fn ec(curve: EcCurve, explicit: bool) -> AsymKey {
    let (d, point) = curve.generate();
    AsymKey::Ec(EcKey {
        curve,
        point,
        d: Some(d),
        explicit,
    })
}

/// An Ed25519 / Ed448 / X25519 / X448 key (`kind` as `asymmetricKeyType`).
pub fn okp(kind: &str) -> KResult<AsymKey> {
    let (len, ctor): (usize, fn(OkpKey) -> AsymKey) = match kind {
        "ed25519" => (32, AsymKey::Ed25519),
        "ed448" => (57, AsymKey::Ed448),
        "x25519" => (32, AsymKey::X25519),
        "x448" => (56, AsymKey::X448),
        _ => return Err(SendError::new("Error", "Unsupported key type")),
    };
    let mut private = vec![0u8; len];
    lumen_os::proc::entropy(&mut private).map_err(|e| SendError::new("Error", e.to_string()))?;
    let public = okp_public(kind, &private)?;
    Ok(ctor(OkpKey {
        public,
        private: Some(private),
    }))
}

/// A safe prime `p = 2q + 1` of `bits` bits.
pub fn safe_prime(bits: u32) -> KResult<BigUint> {
    if bits < 2 {
        return Err(SendError::new(
            "Error",
            "error:1C80006B:Provider routines::modulus too small",
        )
        .with_code("ERR_OSSL_MODULUS_TOO_SMALL"));
    }
    if bits < 3 {
        return Ok(BigUint::from(3u8));
    }
    if bits < 6 {
        let one = BigUint::from(1u8);
        loop {
            let q = OsRng.gen_prime(bits as usize - 1);
            let p = (&q << 1usize) + &one;
            if p.bits() == bits as usize && probably_prime(&p, 20) {
                return Ok(p);
            }
        }
    }
    // Every safe prime above 7 is 11 mod 12.
    Ok(safe_prime_congruent(bits, 12, 11))
}

/// Odd primes below 2^15, for sieving safe-prime candidates.
fn sieve_primes() -> Vec<u32> {
    const LIMIT: usize = 1 << 15;
    let mut composite = vec![false; LIMIT];
    let mut out = Vec::new();
    for i in 3..LIMIT {
        if composite[i] || i % 2 == 0 {
            continue;
        }
        out.push(i as u32);
        let mut j = i * i;
        while j < LIMIT {
            composite[j] = true;
            j += i;
        }
    }
    out
}

/// A safe prime `p` (with `(p - 1) / 2` prime) of exactly `bits` bits (`bits >= 6`) and
/// `p ≡ rem (mod add)`, where `add` is a multiple of 12 or 12 itself and `rem ≡ 11 (mod 12)`.
/// Like OpenSSL's search: walk `p0, p0 + add, ...` from a random start, sieving out candidates
/// where `p` or `(p - 1) / 2` has a small factor, then a base-2 Fermat test on `q` and `p`, and
/// Miller-Rabin on `q`. Once `q` is prime, `2^(p-1) ≡ 1 (mod p)` proves `p` prime (Pocklington,
/// as `gcd(2^2 - 1, p) = 1` for `p ≡ 2 (mod 3)`).
pub fn safe_prime_congruent(bits: u32, add: u32, rem: u32) -> BigUint {
    let primes = sieve_primes();
    let one = BigUint::from(1u8);
    let two = BigUint::from(2u8);
    let add_big = BigUint::from(add);
    let bits = bits as usize;
    loop {
        let r = OsRng.gen_biguint(bits) | (&one << (bits - 1)) | (&one << (bits - 2));
        let p0 = &r - (&r % &add_big) + BigUint::from(rem);
        if p0.bits() != bits {
            continue;
        }
        let residues: Vec<u32> = primes
            .iter()
            .map(|&q| {
                (&p0 % BigUint::from(q))
                    .to_bytes_be()
                    .iter()
                    .fold(0u32, |acc, &b| (acc << 8) | b as u32)
            })
            .collect();
        let steps: Vec<u32> = primes.iter().map(|&q| add % q).collect();
        let mut k: u64 = 0;
        while k < (1 << 20) {
            let sieved = primes.iter().enumerate().any(|(i, &q)| {
                let m = ((residues[i] as u64 + k * steps[i] as u64) % q as u64) as u32;
                // p ≡ 0 divides p; p ≡ 1 means q | (p - 1) / 2. A prime p equal to the small prime itself
                // cannot occur at these sizes.
                m == 0 || m == 1
            });
            if !sieved {
                let p = &p0 + BigUint::from(k) * &add_big;
                if p.bits() != bits {
                    break;
                }
                let q = &p >> 1usize;
                if two.modpow(&(&q - &one), &q) == one
                    && two.modpow(&(&p - &one), &p) == one
                    && probably_prime(&q, 20)
                {
                    return p;
                }
            }
            k += 1;
        }
    }
}

/// A DH key over `(p, g)`: a random private value in `[2, p - 2]`.
pub fn dh(p: BigUint, g: BigUint) -> KResult<AsymKey> {
    let two = BigUint::from(2u8);
    if p <= BigUint::from(3u8) {
        return Err(SendError::new(
            "Error",
            "error:1C80006B:Provider routines::modulus too small",
        )
        .with_code("ERR_OSSL_MODULUS_TOO_SMALL"));
    }
    let upper = &p - &two;
    let x = OsRng.gen_biguint_range(&two, &upper);
    let y = g.modpow(&x, &p);
    Ok(AsymKey::Dh(DhKey {
        p,
        g,
        q: None,
        y,
        x: Some(x),
    }))
}
