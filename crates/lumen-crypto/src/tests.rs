//! Behaviour tests. They run on the backend `LUMEN_CRYPTO_BACKEND` selects (OpenSSL when it loads
//! and nothing is forced); the `cross_*` tests exercise both backends against each other.

use super::*;
use lumen_common::hash::digest;

// 1024-bit key and the vectors below were produced by `openssl` 3 (genpkey, dgst -sign,
// pkeyutl -encrypt / -sign) over MSG and PLAIN. The key is PKCS#8 DER.
const KEY: &str = "30820275020100300d06092a864886f70d01010105000482025f3082025b020100028181009b4d0cbcf4516e3ab45c174c9b721c476b3a12b6b1ff54e748c0d519949972fbf2b6127e33875fb69313a6c0b3ee67962eaa5c2d94849a2404637acc7be242efc834b7a35520fb4e9ea73ccaefb6487cdaf979218eac2d7dc5e53a98ed1a7b0637fecb2b20f1dacc7b92db8ea30026374ce50b7394a2193720e4f04524585feb02030100010281806821f9f0994220cf6c3073cf024c397a0a041e9832322b140a4c82976c74980d2869bd6cb1d08bf538196d2eb9779a2db18cb9d2364bd3af62e1f16d3b8433acd8f37f8585f5fde4669e577534f1826c9146e1e6274a20f0373ee37ade4952d9b422bc11ce44079aa24cc4322a83f38163c6887e3ef462dcfc0675448972d081024100cbd484d19dd6d7805fae3f1f80687a5c26c3bd055ef8d52cb0e3d44f4da8823b7e13891a8b03d64b12bb508580176219aeddc3fca5c732a7b487e03eafc02d29024100c30cc5f1cb72f1756419c3e7db03d8dba924fc357c5d1ff5391530d259d7011dd415c2aae1653eb8ff78d9d9bf2f9dea4f444e795c14d1dc1838cbeb0f2fb2f3024078d91f864856c09e541c3340b2193fb2b3290a40ccf62b2dbc12d825cc9d43d991fe732351988ccd25e5f8efadea2f379afdd0dd524cb033ce4d611ec984df4102402f3919779815452671ccd7247c5f2b12cb99dbc22b50f49acf6e34fac8ab8866ab6175571fe8fe4d95f4b171c99b02b5a6e957c2842bba3f7a51cdf524211d4702406eb990cd9f475f3c2560e8994e6380d1ad29f3dab8ead1089cb218f369c0dd18942cae83b3ee9c8a057ab89a7653929dc66e264234e380db9695da331332f8f9";
const MSG: &[u8] = b"lumen rsa vectors";
const PLAIN: &[u8] = b"oaep secret";
const PKCS1_SHA256: &str = "4684dcaf60d92662eba75673a2b818fd35da7aa04350add326c0ed80967fc8bdbeae98fb62411e8adaffd30f280f23cbc05b1d77ca68ffcf7246a16175eb156b24fa31af7a524aa9063528c3e9be026a41e36840d3dfa2b131d4162f9d8347615c5164a88badb8915585534d46d5b5f48e38dc2b2151bf739a98a5b00bd3ccf5";
const PSS_SALT20: &str = "22a3b885c038fe97a2f6a75f0e8ffc77ad0e3cfe76f997d0d47ba3ab7c24f61943d19d7fb3e891be7c3618369a0725e040057b661320d698dbc62a84eac19aeb7dafc433890f8cd224d13fbccd6adbb79dd85514e7242e1ba6c62813db977763dc4a03a7f59f2d6092601fad1e1f64fa7086f4bbc5f14dec7e5bf90d0d3fb96d";
const PSS_MGF1_SHA1: &str = "0c0323a4283101cd2d59917922e26234accd56f475bf20248017eee43376e82877cc288486a1c0cdbd864f6e89d1602fceb4b189dd283a25acdc6b6ad1ebc9e6d8e102d8ded6a950a513885eb2d4acdbea61f4887ed305b97c7c1e0efabde1164f8c6d830909ecce77622992bd2971d9454aab0dc6cd66ee750b145a199d218d";
const OAEP_SHA256_LABEL: &str = "6ec8febf285d7c1e9caaa605ad27adae951ce69d5cbd892facf6cb3d7ba953c28ca0c355dd462f4fcd8b35a477ad8cfe84f7353c9ed04fb609c00b68418cce206de3f84f66bb4bc401f321b6d0ff764893030f6f67d492ca5fb284de022b7cdfc541bda0bb642b28284d08618054fbdcdcc7adadb6a381da402a4fa2c0e075d5";
const PKCS1_ENCRYPTED: &str = "8773e3682e0aa4ad7925face33acea657ff41ab4f7b5ae9badbf90e26670f7b4afb3d7b4b51faba3d051b653b4fbac31d7c41dfc58262ceeadb1c0b0244229734b9a86ffe28f46dec17b918dacb3f4b80132750c3b25eb3f8ad31b200dbc6305d533a0b3b858dbd77f58e8497a3b8afe8dd8863e9c70b8a0911bb7cb3989ba7d";
const PRIVATE_ENCRYPTED: &str = "8402076ded2788f9645b875445f0d7d696a540e20b69834f7b408a7e38867adc713dfcae5e82648ffd1451c483e8026f80127fd9b0a78321f3ede7e791919018a1b37e021be5c8a2e7d7ec9b90bf978f91fe8632fef63bf0d5399f4d101df2aeca5b01fb46e70db13e87d9c0f3f2a51383df2ad8be224449673df8667391f81b";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn tlv(input: &[u8]) -> (&[u8], &[u8]) {
    let (len, header) = match input[1] {
        l if l < 0x80 => (l as usize, 2),
        0x81 => (input[2] as usize, 3),
        _ => (((input[2] as usize) << 8) | input[3] as usize, 4),
    };
    (&input[header..header + len], &input[header + len..])
}

fn key() -> RsaPrivateKey {
    let der = hex(KEY);
    let (pkcs8, _) = tlv(&der);
    let (_version, rest) = tlv(pkcs8);
    let (_algorithm, rest) = tlv(rest);
    let (octets, _) = tlv(rest);
    let (rsa, _) = tlv(octets);
    let mut ints = Vec::new();
    let mut rest = rsa;
    while !rest.is_empty() {
        let (value, next) = tlv(rest);
        ints.push(trim_be(value).to_vec());
        rest = next;
    }
    RsaPrivateKey {
        public: RsaPublicKey { n: ints[1].clone(), e: ints[2].clone() },
        d: ints[3].clone(),
        p: ints[4].clone(),
        q: ints[5].clone(),
        dp: ints[6].clone(),
        dq: ints[7].clone(),
        qi: ints[8].clone(),
    }
}

fn be() -> &'static dyn Backend {
    backend()
}

fn both() -> Vec<&'static dyn Backend> {
    let mut all = vec![rustcrypto()];
    if let Ok(openssl) = openssl() {
        all.push(openssl);
    }
    all
}

fn pkcs1(hash: Algo) -> RsaScheme {
    RsaScheme::Pkcs1 { hash }
}

fn pss(hash: Algo, salt: PssSalt) -> RsaScheme {
    RsaScheme::Pss { hash, mgf1: hash, salt }
}

fn oaep(hash: Algo, label: &[u8]) -> RsaPadding {
    RsaPadding::Oaep { hash, mgf1: hash, label: label.to_vec() }
}

#[test]
fn selection_honours_the_environment() {
    match std::env::var("LUMEN_CRYPTO_BACKEND").as_deref() {
        Ok("rustcrypto") => assert_eq!(be().name(), "rustcrypto"),
        Ok("openssl") => assert_eq!(be().name(), "openssl"),
        _ => assert!(matches!(be().name(), "openssl" | "rustcrypto")),
    }
}

#[test]
fn pkcs1_v15_signature_matches_openssl() {
    let k = key();
    let sha256 = digest(Algo::Sha256, MSG);
    assert_eq!(be().rsa_sign(&k, &pkcs1(Algo::Sha256), &sha256).unwrap(), hex(PKCS1_SHA256));
    assert!(be().rsa_verify(&k.public, &pkcs1(Algo::Sha256), &sha256, &hex(PKCS1_SHA256)).unwrap());
    let other = digest(Algo::Sha256, b"other");
    assert!(!be().rsa_verify(&k.public, &pkcs1(Algo::Sha256), &other, &hex(PKCS1_SHA256)).unwrap());
    let sha512 = digest(Algo::Sha512, MSG);
    assert!(!be().rsa_verify(&k.public, &pkcs1(Algo::Sha512), &sha512, &hex(PKCS1_SHA256)).unwrap());
    let mut bad = hex(PKCS1_SHA256);
    bad[40] ^= 0x10;
    assert!(!be().rsa_verify(&k.public, &pkcs1(Algo::Sha256), &sha256, &bad).unwrap());
    assert!(!be().rsa_verify(&k.public, &pkcs1(Algo::Sha256), &sha256, &bad[..100]).unwrap());
}

#[test]
fn pkcs1_v15_signs_every_supported_digest() {
    let k = key();
    for algo in [
        Algo::Sha1,
        Algo::Sha224,
        Algo::Sha384,
        Algo::Sha512,
        Algo::Sha512_256,
        Algo::Sha3_256,
        Algo::Ripemd160,
        Algo::Md5,
        Algo::Sm3,
        Algo::Md5Sha1,
    ] {
        let hashed = digest(algo, MSG);
        let sig = be().rsa_sign(&k, &pkcs1(algo), &hashed).unwrap();
        assert!(be().rsa_verify(&k.public, &pkcs1(algo), &hashed, &sig).unwrap(), "{algo:?}");
    }
}

#[test]
fn pss_signature_from_openssl_verifies() {
    let k = key();
    let sig = hex(PSS_SALT20);
    let hashed = digest(Algo::Sha256, MSG);
    let check = |salt| be().rsa_verify(&k.public, &pss(Algo::Sha256, salt), &hashed, &sig).unwrap();
    assert!(check(PssSalt::Length(20)));
    assert!(check(PssSalt::MaxOrAuto));
    assert!(!check(PssSalt::Digest));
    assert!(!check(PssSalt::Length(21)));
    let other = digest(Algo::Sha256, b"other");
    assert!(!be().rsa_verify(&k.public, &pss(Algo::Sha256, PssSalt::MaxOrAuto), &other, &sig).unwrap());
}

#[test]
fn pss_roundtrip_salt_lengths() {
    let k = key();
    let hashed = digest(Algo::Sha256, MSG);
    let max = 128 - 32 - 2;
    for (salt, expect_len) in [(PssSalt::Length(0), 0), (PssSalt::Digest, 32), (PssSalt::MaxOrAuto, max), (PssSalt::Length(7), 7)] {
        let sig = be().rsa_sign(&k, &pss(Algo::Sha256, salt), &hashed).unwrap();
        assert!(be().rsa_verify(&k.public, &pss(Algo::Sha256, PssSalt::MaxOrAuto), &hashed, &sig).unwrap());
        assert!(be().rsa_verify(&k.public, &pss(Algo::Sha256, PssSalt::Length(expect_len)), &hashed, &sig).unwrap());
        assert!(!be().rsa_verify(&k.public, &pss(Algo::Sha256, PssSalt::Length(expect_len + 1)), &hashed, &sig).unwrap());
    }
    assert!(be().rsa_sign(&k, &pss(Algo::Sha256, PssSalt::Length(max + 1)), &hashed).is_err());
}

#[test]
fn pss_with_distinct_mgf1_digest() {
    let k = key();
    let split = |salt| RsaScheme::Pss { hash: Algo::Sha256, mgf1: Algo::Sha1, salt };
    let hashed = digest(Algo::Sha256, MSG);
    assert!(be().rsa_verify(&k.public, &split(PssSalt::MaxOrAuto), &hashed, &hex(PSS_MGF1_SHA1)).unwrap());
    let other = digest(Algo::Sha256, b"other");
    assert!(!be().rsa_verify(&k.public, &split(PssSalt::MaxOrAuto), &other, &hex(PSS_MGF1_SHA1)).unwrap());
    let sig = be().rsa_sign(&k, &split(PssSalt::Length(16)), &hashed).unwrap();
    assert!(be().rsa_verify(&k.public, &split(PssSalt::Length(16)), &hashed, &sig).unwrap());
    assert!(be().rsa_verify(&k.public, &split(PssSalt::MaxOrAuto), &hashed, &sig).unwrap());
    assert!(!be().rsa_verify(&k.public, &split(PssSalt::Length(15)), &hashed, &sig).unwrap());
}

#[test]
fn oaep_decrypts_openssl_ciphertext_with_label() {
    let k = key();
    let label = [0x00, 0xff, 0x10];
    assert_eq!(be().rsa_decrypt(&k, &oaep(Algo::Sha256, &label), &hex(OAEP_SHA256_LABEL)).unwrap(), PLAIN);
}

#[test]
fn oaep_failures_are_indistinguishable() {
    let k = key();
    let ct = hex(OAEP_SHA256_LABEL);
    let label = [0x00, 0xff, 0x10];
    let message = |padding: RsaPadding, input: &[u8]| be().rsa_decrypt(&k, &padding, input).unwrap_err();
    let wrong_label = message(oaep(Algo::Sha256, b"other"), &ct);
    let wrong_hash = message(oaep(Algo::Sha1, &label), &ct);
    let mut flipped = ct.clone();
    flipped[77] ^= 1;
    let corrupted = message(oaep(Algo::Sha256, &label), &flipped);
    let pkcs1_ct = message(oaep(Algo::Sha256, &[]), &hex(PKCS1_ENCRYPTED));
    assert_eq!(wrong_label.message, "error:02000079:rsa routines::oaep decoding error");
    assert_eq!(wrong_label.code.as_deref(), Some("ERR_OSSL_RSA_OAEP_DECODING_ERROR"));
    assert_eq!(wrong_label, wrong_hash);
    assert_eq!(wrong_label, corrupted);
    assert_eq!(wrong_label, pkcs1_ct);
}

#[test]
fn oaep_roundtrip_digests_and_labels() {
    let k = key();
    for algo in [Algo::Sha1, Algo::Sha224, Algo::Sha256, Algo::Sha384, Algo::Sha3_256, Algo::Ripemd160] {
        let ct = be().rsa_encrypt(&k.public, &oaep(algo, b"label"), b"hello").unwrap();
        assert_eq!(be().rsa_decrypt(&k, &oaep(algo, b"label"), &ct).unwrap(), b"hello");
        assert!(be().rsa_decrypt(&k, &oaep(algo, b""), &ct).is_err());
    }
    let split = RsaPadding::Oaep { hash: Algo::Sha256, mgf1: Algo::Sha1, label: Vec::new() };
    let ct = be().rsa_encrypt(&k.public, &split, b"hello").unwrap();
    assert_eq!(be().rsa_decrypt(&k, &split, &ct).unwrap(), b"hello");
    assert!(be().rsa_decrypt(&k, &oaep(Algo::Sha256, b""), &ct).is_err());
    assert!(be().rsa_encrypt(&k.public, &oaep(Algo::Sha256, &[]), &[0u8; 100]).is_err());
}

#[test]
fn pkcs1_v15_encryption_matches_openssl() {
    let k = key();
    assert_eq!(be().rsa_decrypt(&k, &RsaPadding::Pkcs1, &hex(PKCS1_ENCRYPTED)).unwrap(), PLAIN);
    let ct = be().rsa_encrypt(&k.public, &RsaPadding::Pkcs1, PLAIN).unwrap();
    assert_eq!(be().rsa_decrypt(&k, &RsaPadding::Pkcs1, &ct).unwrap(), PLAIN);
    if be().name() == "rustcrypto" {
        let bad = be().rsa_decrypt(&k, &RsaPadding::Pkcs1, &hex(OAEP_SHA256_LABEL)).unwrap_err();
        let mut flipped = hex(PKCS1_ENCRYPTED);
        flipped[3] ^= 0x40;
        assert!(bad.message.contains("padding check failed"), "{bad}");
        assert_eq!(bad, be().rsa_decrypt(&k, &RsaPadding::Pkcs1, &flipped).unwrap_err());
    }
    assert!(be().rsa_encrypt(&k.public, &RsaPadding::Pkcs1, &[0u8; 120]).is_err());
}

#[test]
fn private_encrypt_public_decrypt() {
    let k = key();
    assert_eq!(be().rsa_private_encrypt(&k, &RsaPadding::Pkcs1, PLAIN).unwrap(), hex(PRIVATE_ENCRYPTED));
    assert_eq!(be().rsa_public_decrypt(&k.public, &RsaPadding::Pkcs1, &hex(PRIVATE_ENCRYPTED)).unwrap(), PLAIN);
    assert!(be().rsa_public_decrypt(&k.public, &RsaPadding::Pkcs1, &hex(PKCS1_ENCRYPTED)).is_err());
}

#[test]
fn raw_rsa_roundtrip_and_range() {
    let k = key();
    let mut block = vec![0x5au8; 128];
    block[0] = 0;
    let ct = be().rsa_encrypt(&k.public, &RsaPadding::None, &block).unwrap();
    assert_eq!(be().rsa_decrypt(&k, &RsaPadding::None, &ct).unwrap(), block);
    assert_eq!(be().rsa_private_encrypt(&k, &RsaPadding::None, &block).unwrap().len(), 128);
    let too_big = vec![0xffu8; 128];
    assert!(be().rsa_encrypt(&k.public, &RsaPadding::None, &too_big).unwrap_err().message.contains("too large for modulus"));
    assert!(be().rsa_encrypt(&k.public, &RsaPadding::None, &block[..100]).unwrap_err().message.contains("too small"));
}

#[test]
fn generated_rsa_key_works() {
    let k = be().rsa_generate(1024, &[1, 0, 1]).unwrap();
    assert_eq!(trim_be(&k.public.n).len(), 128);
    assert_eq!(k.public.e, vec![1, 0, 1]);
    let hashed = digest(Algo::Sha256, MSG);
    let sig = be().rsa_sign(&k, &pkcs1(Algo::Sha256), &hashed).unwrap();
    assert!(be().rsa_verify(&k.public, &pkcs1(Algo::Sha256), &hashed, &sig).unwrap());
    let ct = be().rsa_encrypt(&k.public, &oaep(Algo::Sha1, b""), b"x").unwrap();
    assert_eq!(be().rsa_decrypt(&k, &oaep(Algo::Sha1, b""), &ct).unwrap(), b"x");
    assert!(be().rsa_generate(1024, &[2]).is_err());
    assert_eq!(be().rsa_generate(256, &[1, 0, 1]).unwrap_err().code.as_deref(), Some("ERR_OSSL_KEY_SIZE_TOO_SMALL"));
}

#[test]
fn dsa_roundtrip() {
    let k = be().dsa_generate(1024, Some(160)).unwrap();
    let hashed = digest(Algo::Sha1, MSG);
    let sig = be().dsa_sign(&k, &hashed).unwrap();
    assert!(be().dsa_verify(&k.public, &hashed, &sig).unwrap());
    assert!(!be().dsa_verify(&k.public, &digest(Algo::Sha1, b"other"), &sig).unwrap());
    assert!(!be().dsa_verify(&k.public, &hashed, &sig[..sig.len() - 1]).unwrap());
    let sha256 = digest(Algo::Sha256, MSG);
    let sig = be().dsa_sign(&k, &sha256).unwrap();
    assert!(be().dsa_verify(&k.public, &sha256, &sig).unwrap());
}

fn modp(bits_hex: &str) -> DhParams {
    DhParams { p: hex(bits_hex), g: vec![2] }
}

const MODP1: &str = "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a63a3620ffffffffffffffff";
const MODP2: &str = "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece65381ffffffffffffffff";

#[test]
fn dh_parameters_verify_and_flags() {
    let params = modp(MODP2);
    assert_eq!(be().dh_check(&params).unwrap(), 0);
    let mut composite = params.clone();
    *composite.p.last_mut().unwrap() ^= 0x0e;
    assert_ne!(be().dh_check(&composite).unwrap() & DH_CHECK_P_NOT_PRIME, 0);
    let not_safe = DhParams { p: [vec![0x01], vec![0xff; 65]].concat(), g: vec![2] };
    assert_eq!(be().dh_check(&not_safe).unwrap() & DH_CHECK_P_NOT_SAFE_PRIME, DH_CHECK_P_NOT_SAFE_PRIME);
    assert_ne!(be().dh_check(&DhParams { p: vec![0x17], g: vec![2] }).unwrap() & DH_MODULUS_TOO_SMALL, 0);
    assert_ne!(be().dh_check(&DhParams { p: params.p.clone(), g: vec![1] }).unwrap() & DH_NOT_SUITABLE_GENERATOR, 0);
}

#[test]
fn dh_key_agreement() {
    let params = modp(MODP1);
    let (x1, y1) = be().dh_generate_key(&params, None).unwrap();
    let (x2, y2) = be().dh_generate_key(&params, None).unwrap();
    let s1 = be().dh_compute(&params, &x1, &y2).unwrap();
    let s2 = be().dh_compute(&params, &x2, &y1).unwrap();
    assert_eq!(s1, s2);
    assert_eq!(s1.len(), params.p.len());
    assert_eq!(be().dh_public(&params, &x1).unwrap(), y1);
    let (x, y) = be().dh_generate_key(&params, Some(&x1)).unwrap();
    assert_eq!((trim_be(&x), y), (trim_be(&x1), y1));
}

#[test]
fn dh_public_range_flags() {
    let params = modp(MODP1);
    assert_ne!(be().dh_check_public(&params, &[1]).unwrap() & DH_CHECK_PUBKEY_TOO_SMALL, 0);
    let mut p_minus_1 = params.p.clone();
    *p_minus_1.last_mut().unwrap() -= 1;
    assert_ne!(be().dh_check_public(&params, &p_minus_1).unwrap() & DH_CHECK_PUBKEY_TOO_LARGE, 0);
    assert_eq!(be().dh_check_public(&params, &[0x12, 0x34]).unwrap(), 0);
}

#[test]
fn dh_generated_prime_is_safe_and_suits_the_generator() {
    for g in [2u32, 5] {
        let p = be().dh_generate_prime(512, g).unwrap();
        let flags = be().dh_check(&DhParams { p, g: vec![g as u8] }).unwrap();
        assert_eq!(flags, 0, "g={g}");
    }
}

#[test]
fn primes_generate_and_check() {
    for bits in [8u32, 64, 257] {
        let p = be().prime_generate(bits, false, None, None).unwrap();
        assert_eq!(trim_be(&p).len() * 8 - trim_be(&p)[0].leading_zeros() as usize, bits as usize);
        assert!(be().prime_check(&p, 0).unwrap());
    }
    let safe = be().prime_generate(64, true, None, None).unwrap();
    assert!(be().prime_check(&safe, 0).unwrap());
    let mut half = safe.clone();
    let mut carry = 0u8;
    for byte in half.iter_mut() {
        let next = *byte & 1;
        *byte = (*byte >> 1) | (carry << 7);
        carry = next;
    }
    assert!(be().prime_check(&half, 0).unwrap());
    for composite in [vec![0u8], vec![1], vec![4], vec![0x02, 0x31], vec![0xc4, 0x81, 0xc7]] {
        assert!(!be().prime_check(&composite, 0).unwrap(), "{composite:?}");
    }
    for prime in [vec![2u8], vec![3], vec![0x1e, 0xef], vec![0x7f, 0xff, 0xff, 0xff]] {
        assert!(be().prime_check(&prime, 0).unwrap(), "{prime:?}");
    }
}

#[test]
fn prime_with_add_and_rem() {
    let p = be().prime_generate(64, false, Some(&[0x0c]), Some(&[0x01])).unwrap();
    let low = p.iter().fold(0u32, |acc, &b| (acc * 256 + b as u32) % 12);
    assert_eq!(low, 1);
    assert!(be().prime_generate(1, false, None, None).is_err());
    assert_eq!(
        be().prime_generate(1, false, None, None).unwrap_err().code.as_deref(),
        Some("ERR_OSSL_BN_BITS_TOO_SMALL")
    );
}

#[test]
fn cross_backend_rsa_signatures_and_encryption() {
    let backends = both();
    let k = key();
    let hashed = digest(Algo::Sha256, MSG);
    for signer in &backends {
        for verifier in &backends {
            for scheme in [
                pkcs1(Algo::Sha256),
                pss(Algo::Sha256, PssSalt::Length(20)),
                pss(Algo::Sha256, PssSalt::MaxOrAuto),
                RsaScheme::Pss { hash: Algo::Sha256, mgf1: Algo::Sha1, salt: PssSalt::Digest },
            ] {
                let sig = signer.rsa_sign(&k, &scheme, &hashed).unwrap();
                assert!(
                    verifier.rsa_verify(&k.public, &scheme, &hashed, &sig).unwrap(),
                    "{} signed, {} verified {scheme:?}",
                    signer.name(),
                    verifier.name()
                );
            }
            let padding = RsaPadding::Oaep { hash: Algo::Sha384, mgf1: Algo::Sha1, label: b"cross".to_vec() };
            let ct = signer.rsa_encrypt(&k.public, &padding, b"cross backend").unwrap();
            assert_eq!(verifier.rsa_decrypt(&k, &padding, &ct).unwrap(), b"cross backend");
        }
    }
}

#[test]
fn cross_backend_dsa_and_dh() {
    let backends = both();
    for signer in &backends {
        let k = signer.dsa_generate(1024, None).unwrap();
        let hashed = digest(Algo::Sha256, MSG);
        let sig = signer.dsa_sign(&k, &hashed).unwrap();
        for verifier in &backends {
            assert!(verifier.dsa_verify(&k.public, &hashed, &sig).unwrap(), "{} -> {}", signer.name(), verifier.name());
        }
    }
    let params = modp(MODP1);
    let (x, y) = backends[0].dh_generate_key(&params, None).unwrap();
    for backend in &backends {
        assert_eq!(backend.dh_public(&params, &x).unwrap(), y);
        let (x2, y2) = backend.dh_generate_key(&params, None).unwrap();
        let secrets: Vec<_> = backends.iter().map(|b| b.dh_compute(&params, &x2, &y).unwrap()).collect();
        assert!(secrets.windows(2).all(|w| w[0] == w[1]));
        let other: Vec<_> = backends.iter().map(|b| b.dh_compute(&params, &x, &y2).unwrap()).collect();
        assert_eq!(secrets[0], other[0]);
    }
}
