//! The Windows backend: RSA and DSA through CNG (`bcrypt.dll`), on the shared
//! [`System`](crate::system::System) logic.
//!
//! Native: RSA PKCS#1 v1.5 and raw encryption, OAEP with a label and one digest for the hash and
//! MGF1 (SHA-1, SHA-256, SHA-384, SHA-512), PKCS#1 v1.5 signatures over every digest
//! `lumen_common::hash` has (the `DigestInfo` is built by the shared code and signed as a block
//! with `BCRYPT_PKCS1_PADDING_INFO::pszAlgId = NULL`), PSS with any salt length and one digest for
//! the message and MGF1, the raw private and public operations (`BCRYPT_PAD_NONE`), RSA key
//! generation (multiples of 64 bits from 512 to 16384, `e = 65537`), and DSA signatures and
//! verification for 160-bit (FIPS 186-2) and 256-bit (FIPS 186-3) subgroup orders up to 3072-bit
//! moduli.
//!
//! Built on the raw operation: PSS with an MGF1 digest other than the message digest or automatic
//! salt detection while verifying.
//!
//! Handed to the fallback (RustCrypto, when the build has it; otherwise Node's "unsupported
//! operation" error): OAEP with SHA-224, SHA-3 or other digests CNG lacks, or with an MGF1 digest
//! other than the label digest; RSA keys CNG refuses to import or generate; DSA key generation and
//! other DSA parameter sizes; finite-field Diffie-Hellman (CNG's `DHPRIVATEBLOB` insists on the
//! public value, which the backend trait does not carry, and there is no modular exponentiation
//! to compute it), safe-prime generation and primality testing.
//!
//! Not run in CI of this repository: it is compile-checked for `x86_64-pc-windows-msvc`.

mod error;

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::core::PCWSTR;
use windows_sys::Win32::Security::Cryptography::{
    BCryptDecrypt, BCryptDestroyKey, BCryptEncrypt, BCryptExportKey, BCryptFinalizeKeyPair, BCryptGenerateKeyPair, BCryptImportKeyPair,
    BCryptOpenAlgorithmProvider, BCryptSignHash, BCryptVerifySignature, BCRYPT_ALG_HANDLE, BCRYPT_DSA_ALGORITHM,
    BCRYPT_DSA_PRIVATE_MAGIC, BCRYPT_DSA_PRIVATE_MAGIC_V2, BCRYPT_DSA_PUBLIC_MAGIC, BCRYPT_DSA_PUBLIC_MAGIC_V2, BCRYPT_KEY_HANDLE,
    BCRYPT_OAEP_PADDING_INFO, BCRYPT_PAD_NONE, BCRYPT_PAD_OAEP, BCRYPT_PAD_PKCS1, BCRYPT_PAD_PSS, BCRYPT_PKCS1_PADDING_INFO,
    BCRYPT_PSS_PADDING_INFO, BCRYPT_RSAFULLPRIVATE_BLOB, BCRYPT_RSAFULLPRIVATE_MAGIC, BCRYPT_RSAPUBLIC_BLOB, BCRYPT_RSAPUBLIC_MAGIC,
    BCRYPT_RSA_ALGORITHM, BCRYPT_SHA1_ALGORITHM, BCRYPT_SHA256_ALGORITHM, BCRYPT_SHA384_ALGORITHM, BCRYPT_SHA512_ALGORITHM,
    BCRYPT_DSA_PRIVATE_BLOB, BCRYPT_DSA_PUBLIC_BLOB,
};

use crate::error::CryptoError;
use crate::rsa_util::{bit_len, mod_len};
use crate::system::{Native, NativeError, Primitives, System};
use crate::{der, pad_be, pss, trim_be, Algo, Backend, DsaPrivateKey, DsaPublicKey, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey};
use error::Context;

const PUBLIC_EXPONENT: [u8; 3] = [1, 0, 1];
const DSA_V2_SHA256: u32 = 1;
const DSA_FIPS186_3: u32 = 1;
const MAX_DSA_MODULUS_BITS: usize = 3072;

struct Cng {
    rsa: Option<Provider>,
    dsa: Option<Provider>,
}

pub(crate) fn backend() -> &'static dyn Backend {
    static BACKEND: OnceLock<System<Cng>> = OnceLock::new();
    BACKEND.get_or_init(|| System::new(Cng { rsa: Provider::open(BCRYPT_RSA_ALGORITHM), dsa: Provider::open(BCRYPT_DSA_ALGORITHM) }, crate::rustcrypto().ok()))
}

/// An algorithm provider handle; CNG allows using it from any thread.
struct Provider(BCRYPT_ALG_HANDLE);

unsafe impl Send for Provider {}
unsafe impl Sync for Provider {}

impl Provider {
    fn open(algorithm: PCWSTR) -> Option<Provider> {
        let mut handle: BCRYPT_ALG_HANDLE = std::ptr::null_mut();
        let status = unsafe { BCryptOpenAlgorithmProvider(&mut handle, algorithm, std::ptr::null(), 0) };
        (status >= 0).then_some(Provider(handle))
    }

    fn import(&self, blob_type: PCWSTR, blob: &[u8]) -> Native<Key> {
        let mut handle: BCRYPT_KEY_HANDLE = std::ptr::null_mut();
        let status = unsafe { BCryptImportKeyPair(self.0, std::ptr::null_mut(), blob_type, &mut handle, blob.as_ptr(), blob.len() as u32, 0) };
        if status >= 0 {
            Ok(Key(handle))
        } else {
            Err(NativeError::Rejected)
        }
    }
}

/// An imported or generated key, destroyed on drop.
struct Key(BCRYPT_KEY_HANDLE);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { BCryptDestroyKey(self.0) };
    }
}

fn check(context: Context, status: i32) -> Native<()> {
    if status >= 0 {
        Ok(())
    } else {
        Err(NativeError::Failed(error::map(context, status)))
    }
}

fn be32(blob: &mut Vec<u8>, value: u32) {
    blob.extend_from_slice(&value.to_le_bytes());
}

fn read_le32(blob: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(blob.get(at..at + 4)?.try_into().ok()?) as usize)
}

fn rsa_blob_header(magic: u32, n: &[u8], e: &[u8], prime1: usize, prime2: usize) -> Vec<u8> {
    let mut blob = Vec::new();
    be32(&mut blob, magic);
    be32(&mut blob, bit_len(n) as u32);
    be32(&mut blob, e.len() as u32);
    be32(&mut blob, mod_len(n) as u32);
    be32(&mut blob, prime1 as u32);
    be32(&mut blob, prime2 as u32);
    blob
}

fn rsa_public_blob(k: &RsaPublicKey) -> Vec<u8> {
    let e = trim_be(&k.e);
    let mut blob = rsa_blob_header(BCRYPT_RSAPUBLIC_MAGIC, &k.n, e, 0, 0);
    blob.extend_from_slice(e);
    blob.extend_from_slice(trim_be(&k.n));
    blob
}

fn rsa_private_blob(k: &RsaPrivateKey) -> Vec<u8> {
    let e = trim_be(&k.public.e);
    let (p, q) = (trim_be(&k.p), trim_be(&k.q));
    let mut blob = rsa_blob_header(BCRYPT_RSAFULLPRIVATE_MAGIC, &k.public.n, e, p.len(), q.len());
    blob.extend_from_slice(e);
    for (value, len) in [
        (&k.public.n, mod_len(&k.public.n)),
        (&k.p, p.len()),
        (&k.q, q.len()),
        (&k.dp, p.len()),
        (&k.dq, q.len()),
        (&k.qi, p.len()),
        (&k.d, mod_len(&k.public.n)),
    ] {
        blob.extend_from_slice(&pad_be(value, len));
    }
    blob
}

fn parse_rsa_private_blob(blob: &[u8]) -> Option<RsaPrivateKey> {
    let (e_len, n_len, p_len, q_len) = (read_le32(blob, 8)?, read_le32(blob, 12)?, read_le32(blob, 16)?, read_le32(blob, 20)?);
    let mut at = 24;
    let mut take = |len: usize| {
        let part = blob.get(at..at + len).map(|b| trim_be(b).to_vec());
        at += len;
        part
    };
    let e = take(e_len)?;
    let n = take(n_len)?;
    let p = take(p_len)?;
    let q = take(q_len)?;
    let dp = take(p_len)?;
    let dq = take(q_len)?;
    let qi = take(p_len)?;
    let d = take(n_len)?;
    Some(RsaPrivateKey { public: RsaPublicKey { n, e }, d, p, q, dp, dq, qi })
}

fn rsa_public_key(cng: &Cng, k: &RsaPublicKey) -> Native<Key> {
    cng.rsa.as_ref().ok_or(NativeError::Rejected)?.import(BCRYPT_RSAPUBLIC_BLOB, &rsa_public_blob(k))
}

fn rsa_private_key(cng: &Cng, k: &RsaPrivateKey) -> Native<Key> {
    cng.rsa.as_ref().ok_or(NativeError::Rejected)?.import(BCRYPT_RSAFULLPRIVATE_BLOB, &rsa_private_blob(k))
}

/// The CNG name of a digest usable for OAEP and PSS.
fn hash_name(algo: Algo) -> Option<PCWSTR> {
    Some(match algo {
        Algo::Sha1 => BCRYPT_SHA1_ALGORITHM,
        Algo::Sha256 => BCRYPT_SHA256_ALGORITHM,
        Algo::Sha384 => BCRYPT_SHA384_ALGORITHM,
        Algo::Sha512 => BCRYPT_SHA512_ALGORITHM,
        _ => return None,
    })
}

/// Runs `BCryptEncrypt` or `BCryptDecrypt` with an output as long as the modulus can make it.
fn crypt(encrypt: bool, key: &Key, input: &[u8], info: *const c_void, flags: u32, out_len: usize) -> Native<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut written = 0u32;
    let status = unsafe {
        let (iv, iv_len) = (std::ptr::null_mut(), 0);
        if encrypt {
            BCryptEncrypt(key.0, input.as_ptr(), input.len() as u32, info, iv, iv_len, out.as_mut_ptr(), out.len() as u32, &mut written, flags)
        } else {
            BCryptDecrypt(key.0, input.as_ptr(), input.len() as u32, info, iv, iv_len, out.as_mut_ptr(), out.len() as u32, &mut written, flags)
        }
    };
    check(Context::Crypt, status)?;
    out.truncate(written as usize);
    Ok(out)
}

fn padded_crypt(encrypt: bool, key: &Key, padding: &RsaPadding, input: &[u8], out_len: usize) -> Native<Vec<u8>> {
    match padding {
        RsaPadding::Pkcs1 => crypt(encrypt, key, input, std::ptr::null(), BCRYPT_PAD_PKCS1, out_len),
        RsaPadding::None => crypt(encrypt, key, input, std::ptr::null(), BCRYPT_PAD_NONE, out_len),
        RsaPadding::Oaep { hash, label, .. } => {
            let algorithm = hash_name(*hash).ok_or(NativeError::Rejected)?;
            let info = BCRYPT_OAEP_PADDING_INFO {
                pszAlgId: algorithm,
                pbLabel: if label.is_empty() { std::ptr::null_mut() } else { label.as_ptr() as *mut u8 },
                cbLabel: label.len() as u32,
            };
            crypt(encrypt, key, input, &info as *const _ as *const c_void, BCRYPT_PAD_OAEP, out_len)
        }
    }
}

fn sign(key: &Key, info: *const c_void, hashed: &[u8], flags: u32, out_len: usize) -> Native<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut written = 0u32;
    let status = unsafe { BCryptSignHash(key.0, info, hashed.as_ptr(), hashed.len() as u32, out.as_mut_ptr(), out.len() as u32, &mut written, flags) };
    check(Context::Sign, status)?;
    out.truncate(written as usize);
    Ok(out)
}

fn verify(key: &Key, info: *const c_void, hashed: &[u8], signature: &[u8], flags: u32) -> bool {
    let status = unsafe { BCryptVerifySignature(key.0, info, hashed.as_ptr(), hashed.len() as u32, signature.as_ptr(), signature.len() as u32, flags) };
    status >= 0
}

fn dsa_group_len(params: &crate::DsaParams) -> Option<usize> {
    let (q, p) = (trim_be(&params.q), trim_be(&params.p));
    (matches!(q.len(), 20 | 32) && bit_len(p) <= MAX_DSA_MODULUS_BITS).then_some(q.len())
}

/// A DSA key blob: FIPS 186-2 (`DSAB` / `DSAV`) for 20-byte subgroup orders, FIPS 186-3 (`DPB2` /
/// `DPV2`) for 32-byte ones. The seed is not part of a key's value, so it is filled with `0xFF` and
/// the counter with `-1`, which CNG accepts.
fn dsa_blob(public: &DsaPublicKey, private: Option<&[u8]>, group: usize) -> Vec<u8> {
    let params = &public.params;
    let key_len = mod_len(&params.p);
    let mut blob = Vec::new();
    if group == 20 {
        be32(&mut blob, if private.is_some() { BCRYPT_DSA_PRIVATE_MAGIC } else { BCRYPT_DSA_PUBLIC_MAGIC });
        be32(&mut blob, key_len as u32);
        blob.extend_from_slice(&[0xff; 4]);
        blob.extend_from_slice(&[0xff; 20]);
    } else {
        be32(&mut blob, if private.is_some() { BCRYPT_DSA_PRIVATE_MAGIC_V2 } else { BCRYPT_DSA_PUBLIC_MAGIC_V2 });
        be32(&mut blob, key_len as u32);
        be32(&mut blob, DSA_V2_SHA256);
        be32(&mut blob, DSA_FIPS186_3);
        be32(&mut blob, group as u32);
        be32(&mut blob, group as u32);
        blob.extend_from_slice(&[0xff; 4]);
        blob.extend_from_slice(&vec![0xff; group]);
    }
    blob.extend_from_slice(&pad_be(&params.q, group));
    for (value, len) in [(&params.p, key_len), (&params.g, key_len), (&public.y, key_len)] {
        blob.extend_from_slice(&pad_be(value, len));
    }
    if let Some(x) = private {
        blob.extend_from_slice(&pad_be(x, group));
    }
    blob
}

/// The digest as a `group`-byte big-endian integer: truncated to the subgroup order's length or
/// left-padded, which is what FIPS 186 does with a hash of another size.
fn dsa_digest(hashed: &[u8], group: usize) -> Vec<u8> {
    if hashed.len() >= group {
        hashed[..group].to_vec()
    } else {
        pad_be(hashed, group)
    }
}

fn dsa_key(cng: &Cng, public: &DsaPublicKey, private: Option<&[u8]>) -> Option<(Native<Key>, usize)> {
    let group = dsa_group_len(&public.params)?;
    let provider = cng.dsa.as_ref()?;
    let (kind, blob) = (
        if private.is_some() { BCRYPT_DSA_PRIVATE_BLOB } else { BCRYPT_DSA_PUBLIC_BLOB },
        dsa_blob(public, private, group),
    );
    Some((provider.import(kind, &blob), group))
}

impl Primitives for Cng {
    const NAME: &'static str = "cng";

    fn oaep(&self, hash: Algo, mgf1: Algo, _label: &[u8]) -> bool {
        hash == mgf1 && hash_name(hash).is_some()
    }

    fn encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>> {
        padded_crypt(true, &rsa_public_key(self, key)?, padding, input, mod_len(&key.n))
    }

    fn decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Native<Vec<u8>> {
        padded_crypt(false, &rsa_private_key(self, key)?, padding, input, mod_len(&key.public.n))
    }

    fn raw_public(&self, key: &RsaPublicKey, block: &[u8]) -> Native<Vec<u8>> {
        padded_crypt(true, &rsa_public_key(self, key)?, &RsaPadding::None, block, mod_len(&key.n))
    }

    fn raw_private(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>> {
        padded_crypt(false, &rsa_private_key(self, key)?, &RsaPadding::None, block, mod_len(&key.public.n))
    }

    fn sign_block(&self, key: &RsaPrivateKey, block: &[u8]) -> Native<Vec<u8>> {
        let info = BCRYPT_PKCS1_PADDING_INFO { pszAlgId: std::ptr::null() };
        sign(&rsa_private_key(self, key)?, &info as *const _ as *const c_void, block, BCRYPT_PAD_PKCS1, mod_len(&key.public.n))
    }

    fn verify_block(&self, key: &RsaPublicKey, block: &[u8], signature: &[u8]) -> Native<bool> {
        let info = BCRYPT_PKCS1_PADDING_INFO { pszAlgId: std::ptr::null() };
        Ok(verify(&rsa_public_key(self, key)?, &info as *const _ as *const c_void, block, signature, BCRYPT_PAD_PKCS1))
    }

    fn pss_salt(&self, hash: Algo, mgf1: Algo, salt: PssSalt, n: &[u8], signing: bool) -> Option<usize> {
        if hash != mgf1 || hash_name(hash).is_none() {
            return None;
        }
        match salt {
            PssSalt::Digest => Some(hash.out_len()),
            PssSalt::Length(len) => Some(len as usize),
            PssSalt::MaxOrAuto if signing => pss::sign_salt_len(n, hash, salt).ok(),
            PssSalt::MaxOrAuto => None,
        }
    }

    fn pss_sign(&self, key: &RsaPrivateKey, hash: Algo, salt_len: usize, hashed: &[u8]) -> Native<Vec<u8>> {
        let info = BCRYPT_PSS_PADDING_INFO { pszAlgId: hash_name(hash).ok_or(NativeError::Rejected)?, cbSalt: salt_len as u32 };
        sign(&rsa_private_key(self, key)?, &info as *const _ as *const c_void, hashed, BCRYPT_PAD_PSS, mod_len(&key.public.n))
    }

    fn pss_verify(&self, key: &RsaPublicKey, hash: Algo, salt_len: usize, hashed: &[u8], signature: &[u8]) -> Native<bool> {
        let info = BCRYPT_PSS_PADDING_INFO { pszAlgId: hash_name(hash).ok_or(NativeError::Rejected)?, cbSalt: salt_len as u32 };
        Ok(verify(&rsa_public_key(self, key)?, &info as *const _ as *const c_void, hashed, signature, BCRYPT_PAD_PSS))
    }

    fn generate(&self, bits: u32, e: &[u8]) -> Option<Native<RsaPrivateKey>> {
        let provider = self.rsa.as_ref()?;
        if trim_be(e) != PUBLIC_EXPONENT || !(512..=16384).contains(&bits) || bits % 64 != 0 {
            return None;
        }
        Some((|| {
            let mut handle: BCRYPT_KEY_HANDLE = std::ptr::null_mut();
            check(Context::Generate, unsafe { BCryptGenerateKeyPair(provider.0, &mut handle, bits, 0) })?;
            let key = Key(handle);
            check(Context::Generate, unsafe { BCryptFinalizeKeyPair(key.0, 0) })?;
            let mut size = 0u32;
            check(Context::Generate, unsafe {
                BCryptExportKey(key.0, std::ptr::null_mut(), BCRYPT_RSAFULLPRIVATE_BLOB, std::ptr::null_mut(), 0, &mut size, 0)
            })?;
            let mut blob = vec![0u8; size as usize];
            check(Context::Generate, unsafe {
                BCryptExportKey(key.0, std::ptr::null_mut(), BCRYPT_RSAFULLPRIVATE_BLOB, blob.as_mut_ptr(), size, &mut size, 0)
            })?;
            parse_rsa_private_blob(&blob[..size as usize]).ok_or_else(|| NativeError::Failed(CryptoError::error("RSA key generation failed")))
        })())
    }

    fn dsa_sign(&self, key: &DsaPrivateKey, hashed: &[u8]) -> Option<Native<Vec<u8>>> {
        let (imported, group) = dsa_key(self, &key.public, Some(&key.x))?;
        Some(imported.and_then(|handle| {
            let signature = sign(&handle, std::ptr::null(), &dsa_digest(hashed, group), 0, 2 * group)?;
            let (r, s) = signature.split_at(signature.len() / 2);
            Ok(der::dss_signature(r, s))
        }))
    }

    fn dsa_verify(&self, key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Option<Native<bool>> {
        let (imported, group) = dsa_key(self, key, None)?;
        Some(imported.map(|handle| {
            let Some((r, s)) = der::parse_dss_signature(signature) else { return false };
            if r.len() > group || s.len() > group {
                return false;
            }
            let raw = [pad_be(&r, group), pad_be(&s, group)].concat();
            verify(&handle, std::ptr::null(), &dsa_digest(hashed, group), &raw, 0)
        }))
    }
}
