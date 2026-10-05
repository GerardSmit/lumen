//! Symmetric ciphers: the streaming `CipherBase` engine behind `createCipheriv`/`createDecipheriv`,
//! the cipher table behind `getCiphers`/`getCipherInfo`, and the one-shot AES-CTR op WebCrypto
//! needs for its counter-length semantics.
//!
//! The engine follows OpenSSL's EVP semantics (block buffering, PKCS#7 padding, the held-back
//! last block while decrypting, one-shot CCM and key-wrap updates) and reports OpenSSL 3's error
//! strings. Block ciphers, modes and AEADs come from RustCrypto; GCM and ChaCha20-Poly1305 are
//! assembled from their CTR keystream and universal hash so that `update()` streams.

use crate::hash::{self, Algo};
use cipher::consts::{U16, U256};
use cipher::generic_array::GenericArray;
use cipher::inout::InOutBuf;
use cipher::typenum::{IsLess, Le, NonZero};
use cipher::{
    BlockCipher, BlockDecrypt, BlockDecryptMut, BlockEncrypt, BlockEncryptMut, BlockSizeUser,
    KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek,
};
use ghash::universal_hash::UniversalHash;
use ghash::GHash;
use lumen::embed::OpError;
use poly1305::Poly1305;

#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Alg {
        Aes128,
        Aes192,
        Aes256,
        Tdes2,
        Tdes3,
        ChaCha,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Cbc,
        Ecb,
        Cfb,
        Cfb8,
        Ofb,
        Ctr,
        Gcm,
        Ccm,
        Ocb,
        Wrap,
        WrapPad,
        Des3Wrap,
        Stream,
        ChaPoly,
    }

    impl Mode {
        fn label(self) -> &'static str {
            match self {
                Mode::Cbc => "cbc",
                Mode::Ecb => "ecb",
                Mode::Cfb | Mode::Cfb8 => "cfb",
                Mode::Ofb => "ofb",
                Mode::Ctr => "ctr",
                Mode::Gcm => "gcm",
                Mode::Ccm => "ccm",
                Mode::Ocb => "ocb",
                Mode::Wrap | Mode::WrapPad | Mode::Des3Wrap => "wrap",
                Mode::Stream | Mode::ChaPoly => "stream",
            }
        }

        fn is_aead(self) -> bool {
            matches!(self, Mode::Gcm | Mode::Ccm | Mode::Ocb | Mode::ChaPoly)
        }
    }

    struct Spec {
        /// OpenSSL's short name (`OBJ_nid2sn`).
        sn: &'static str,
        nid: i32,
        alg: Alg,
        mode: Mode,
        key: usize,
        iv: usize,
        /// `EVP_CIPHER_block_size`; 0 for stream ciphers (not reported).
        block: usize,
        /// The names `getCiphers()` lists for this cipher.
        names: &'static [&'static str],
    }

    macro_rules! specs {
    ($($sn:literal $nid:literal $alg:ident $mode:ident $key:literal $iv:literal $block:literal [$($n:literal),*];)*) => {
        &[$(Spec { sn: $sn, nid: $nid, alg: Alg::$alg, mode: Mode::$mode, key: $key, iv: $iv, block: $block, names: &[$($n),*] }),*]
    };
}

    const SPECS: &[Spec] = specs! {
        "aes-128-cbc" 419 Aes128 Cbc 16 16 16 ["aes-128-cbc", "aes128"];
        "id-aes128-CCM" 896 Aes128 Ccm 16 12 1 ["aes-128-ccm", "id-aes128-CCM"];
        "aes-128-cfb" 421 Aes128 Cfb 16 16 1 ["aes-128-cfb"];
        "aes-128-cfb8" 653 Aes128 Cfb8 16 16 1 ["aes-128-cfb8"];
        "aes-128-ctr" 904 Aes128 Ctr 16 16 1 ["aes-128-ctr"];
        "aes-128-ecb" 418 Aes128 Ecb 16 0 16 ["aes-128-ecb"];
        "id-aes128-GCM" 895 Aes128 Gcm 16 12 1 ["aes-128-gcm", "id-aes128-GCM"];
        "aes-128-ocb" 958 Aes128 Ocb 16 12 16 ["aes-128-ocb"];
        "aes-128-ofb" 420 Aes128 Ofb 16 16 1 ["aes-128-ofb"];
        "aes-192-cbc" 423 Aes192 Cbc 24 16 16 ["aes-192-cbc", "aes192"];
        "id-aes192-CCM" 899 Aes192 Ccm 24 12 1 ["aes-192-ccm", "id-aes192-CCM"];
        "aes-192-cfb" 425 Aes192 Cfb 24 16 1 ["aes-192-cfb"];
        "aes-192-cfb8" 654 Aes192 Cfb8 24 16 1 ["aes-192-cfb8"];
        "aes-192-ctr" 905 Aes192 Ctr 24 16 1 ["aes-192-ctr"];
        "aes-192-ecb" 422 Aes192 Ecb 24 0 16 ["aes-192-ecb"];
        "id-aes192-GCM" 898 Aes192 Gcm 24 12 1 ["aes-192-gcm", "id-aes192-GCM"];
        "aes-192-ocb" 959 Aes192 Ocb 24 12 16 ["aes-192-ocb"];
        "aes-192-ofb" 424 Aes192 Ofb 24 16 1 ["aes-192-ofb"];
        "aes-256-cbc" 427 Aes256 Cbc 32 16 16 ["aes-256-cbc", "aes256"];
        "id-aes256-CCM" 902 Aes256 Ccm 32 12 1 ["aes-256-ccm", "id-aes256-CCM"];
        "aes-256-cfb" 429 Aes256 Cfb 32 16 1 ["aes-256-cfb"];
        "aes-256-cfb8" 655 Aes256 Cfb8 32 16 1 ["aes-256-cfb8"];
        "aes-256-ctr" 906 Aes256 Ctr 32 16 1 ["aes-256-ctr"];
        "aes-256-ecb" 426 Aes256 Ecb 32 0 16 ["aes-256-ecb"];
        "id-aes256-GCM" 901 Aes256 Gcm 32 12 1 ["aes-256-gcm", "id-aes256-GCM"];
        "aes-256-ocb" 960 Aes256 Ocb 32 12 16 ["aes-256-ocb"];
        "aes-256-ofb" 428 Aes256 Ofb 32 16 1 ["aes-256-ofb"];
        "id-aes128-wrap" 788 Aes128 Wrap 16 8 8 ["aes128-wrap", "id-aes128-wrap"];
        "id-aes128-wrap-pad" 897 Aes128 WrapPad 16 4 8 ["aes128-wrap-pad", "id-aes128-wrap-pad"];
        "id-aes192-wrap" 789 Aes192 Wrap 24 8 8 ["aes192-wrap", "id-aes192-wrap"];
        "id-aes192-wrap-pad" 900 Aes192 WrapPad 24 4 8 ["aes192-wrap-pad", "id-aes192-wrap-pad"];
        "id-aes256-wrap" 790 Aes256 Wrap 32 8 8 ["aes256-wrap", "id-aes256-wrap"];
        "id-aes256-wrap-pad" 903 Aes256 WrapPad 32 4 8 ["aes256-wrap-pad", "id-aes256-wrap-pad"];
        "chacha20" 1019 ChaCha Stream 32 16 0 ["chacha20"];
        "chacha20-poly1305" 1018 ChaCha ChaPoly 32 12 0 ["chacha20-poly1305"];
        "des-ede" 32 Tdes2 Ecb 16 0 8 ["des-ede", "des-ede-ecb"];
        "des-ede-cbc" 43 Tdes2 Cbc 16 8 8 ["des-ede-cbc"];
        "des-ede-cfb" 60 Tdes2 Cfb 16 8 1 ["des-ede-cfb"];
        "des-ede-ofb" 62 Tdes2 Ofb 16 8 1 ["des-ede-ofb"];
        "des-ede3" 33 Tdes3 Ecb 24 0 8 ["des-ede3", "des-ede3-ecb"];
        "des-ede3-cbc" 44 Tdes3 Cbc 24 8 8 ["des-ede3-cbc", "des3"];
        "des-ede3-cfb" 61 Tdes3 Cfb 24 8 1 ["des-ede3-cfb"];
        "des-ede3-cfb8" 659 Tdes3 Cfb8 24 8 1 ["des-ede3-cfb8"];
        "des-ede3-ofb" 63 Tdes3 Ofb 24 8 1 ["des-ede3-ofb"];
        "id-smime-alg-CMS3DESwrap" 246 Tdes3 Des3Wrap 24 0 8 ["des3-wrap", "id-smime-alg-CMS3DESwrap"];
    };

    /// Names OpenSSL resolves without listing them.
    const HIDDEN: &[(&str, i32)] = &[
        ("aes-128-wrap", 788),
        ("aes-192-wrap", 789),
        ("aes-256-wrap", 790),
        ("aes-128-wrap-pad", 897),
        ("aes-192-wrap-pad", 900),
        ("aes-256-wrap-pad", 903),
    ];

    fn by_nid(nid: i32) -> Option<&'static Spec> {
        SPECS.iter().find(|s| s.nid == nid)
    }

    fn lookup(name: &str) -> Option<&'static Spec> {
        SPECS
            .iter()
            .find(|s| {
                s.sn.eq_ignore_ascii_case(name)
                    || s.names.iter().any(|n| n.eq_ignore_ascii_case(name))
            })
            .or_else(|| {
                HIDDEN
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .and_then(|&(_, nid)| by_nid(nid))
            })
    }

    #[op(name = "getCiphers")]
    fn get_ciphers() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = SPECS.iter().flat_map(|s| s.names.iter().copied()).collect();
        v.sort_unstable();
        v
    }

    /// `[name, mode]` and `[nid, blockSize, ivLength, keyLength]` (0 = not reported) of a cipher, or
    /// `null` when it is unknown or the requested key/IV length does not fit it.
    #[op(name = "cipherInfo")]
    fn cipher_info(
        name: Option<String>,
        nid: i32,
        key_len: Option<f64>,
        iv_len: Option<f64>,
    ) -> Option<(String, &'static str, Vec<i32>)> {
        let spec = match name {
            Some(n) => lookup(&n)?,
            None => by_nid(nid)?,
        };
        let mut key = spec.key as i32;
        let mut iv = spec.iv as i32;
        if let Some(k) = key_len {
            if k != spec.key as f64 {
                return None;
            }
            key = k as i32;
        }
        if let Some(n) = iv_len {
            let ok = match spec.mode {
                Mode::Ccm => (7.0..=13.0).contains(&n),
                Mode::Gcm => (1.0..=128.0).contains(&n),
                Mode::Ocb => (1.0..=15.0).contains(&n),
                _ => n == spec.iv as f64,
            };
            if !ok {
                return None;
            }
            iv = n as i32;
        }
        let block = if spec.mode.label() == "stream" {
            0
        } else {
            spec.block as i32
        };
        Some((
            spec.sn.to_string(),
            spec.mode.label(),
            vec![spec.nid, block, iv, key],
        ))
    }

    // ---- errors ------------------------------------------------------------------------------------

    fn ossl(hex: &str, reason: &str, code: &'static str) -> OpError {
        OpError::error(format!("error:{hex}:Provider routines::{reason}")).with_code(code)
    }

    fn wrong_final_block_length() -> OpError {
        ossl(
            "1C80006B",
            "wrong final block length",
            "ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH",
        )
    }

    fn bad_decrypt() -> OpError {
        ossl("1C800064", "bad decrypt", "ERR_OSSL_BAD_DECRYPT")
    }

    fn invalid_iv() -> OpError {
        OpError::type_error("Invalid initialization vector").with_code("ERR_CRYPTO_INVALID_IV")
    }

    fn invalid_keylen() -> OpError {
        OpError::range_error("Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN")
    }

    fn invalid_tag(msg: String) -> OpError {
        OpError::type_error(msg).with_code("ERR_CRYPTO_INVALID_AUTH_TAG")
    }

    fn unsupported_update() -> OpError {
        OpError::error("Trying to add data in unsupported state")
    }

    fn auth_failed() -> OpError {
        OpError::error("Unsupported state or unable to authenticate data")
    }

    // ---- engines -----------------------------------------------------------------------------------

    /// An in-place transform: a keystream for stream modes, whole blocks for block modes.
    type Xform = Box<dyn FnMut(&mut [u8])>;

    fn enc_blocks<T: BlockEncryptMut + 'static>(mut t: T) -> Xform {
        Box::new(move |buf: &mut [u8]| {
            let (blocks, _) = InOutBuf::from(buf).into_chunks();
            t.encrypt_blocks_inout_mut(blocks);
        })
    }

    fn dec_blocks<T: BlockDecryptMut + 'static>(mut t: T) -> Xform {
        Box::new(move |buf: &mut [u8]| {
            let (blocks, _) = InOutBuf::from(buf).into_chunks();
            t.decrypt_blocks_inout_mut(blocks);
        })
    }

    fn keystream<T: StreamCipher + 'static>(mut t: T) -> Xform {
        Box::new(move |buf: &mut [u8]| t.apply_keystream(buf))
    }

    /// CBC/ECB/CFB/CFB8/OFB over any block cipher.
    fn generic_mode<C>(
        mode: Mode,
        enc: bool,
        key: &[u8],
        iv: &[u8],
    ) -> Result<(Xform, usize), OpError>
    where
        C: BlockCipher + BlockEncrypt + BlockDecrypt + KeyInit + Clone + 'static,
        C::BlockSize: IsLess<U256>,
        Le<C::BlockSize, U256>: NonZero,
    {
        let bad = |_| invalid_keylen();
        let bs = C::block_size();
        Ok(match (mode, enc) {
            (Mode::Cbc, true) => (
                enc_blocks(cbc::Encryptor::<C>::new_from_slices(key, iv).map_err(bad)?),
                bs,
            ),
            (Mode::Cbc, false) => (
                dec_blocks(cbc::Decryptor::<C>::new_from_slices(key, iv).map_err(bad)?),
                bs,
            ),
            (Mode::Ecb, true) => (
                enc_blocks(ecb::Encryptor::<C>::new_from_slice(key).map_err(bad)?),
                bs,
            ),
            (Mode::Ecb, false) => (
                dec_blocks(ecb::Decryptor::<C>::new_from_slice(key).map_err(bad)?),
                bs,
            ),
            (Mode::Cfb, true) => {
                let mut e = cfb_mode::BufEncryptor::<C>::new_from_slices(key, iv).map_err(bad)?;
                (Box::new(move |b: &mut [u8]| e.encrypt(b)) as Xform, 1)
            }
            (Mode::Cfb, false) => {
                let mut d = cfb_mode::BufDecryptor::<C>::new_from_slices(key, iv).map_err(bad)?;
                (Box::new(move |b: &mut [u8]| d.decrypt(b)) as Xform, 1)
            }
            (Mode::Cfb8, true) => (
                enc_blocks(cfb8::Encryptor::<C>::new_from_slices(key, iv).map_err(bad)?),
                1,
            ),
            (Mode::Cfb8, false) => (
                dec_blocks(cfb8::Decryptor::<C>::new_from_slices(key, iv).map_err(bad)?),
                1,
            ),
            (Mode::Ofb, _) => (
                keystream(ofb::Ofb::<C>::new_from_slices(key, iv).map_err(bad)?),
                1,
            ),
            _ => {
                return Err(OpError::error("Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER"));
            }
        })
    }

    /// A universal hash fed with arbitrary-length chunks; a partial block waits for more input or `pad`.
    struct Mac<U> {
        u: U,
        buf: [u8; 16],
        n: usize,
    }

    impl<U: UniversalHash<BlockSize = U16>> Mac<U> {
        fn new(u: U) -> Self {
            Mac {
                u,
                buf: [0; 16],
                n: 0,
            }
        }

        fn feed(&mut self, mut d: &[u8]) {
            if self.n > 0 {
                let k = (16 - self.n).min(d.len());
                self.buf[self.n..self.n + k].copy_from_slice(&d[..k]);
                self.n += k;
                d = &d[k..];
                if self.n < 16 {
                    return;
                }
                self.u.update(&[self.buf.into()]);
                self.n = 0;
            }
            let full = d.len() / 16 * 16;
            if full > 0 {
                self.u.update_padded(&d[..full]);
            }
            let rest = &d[full..];
            self.buf[..rest.len()].copy_from_slice(rest);
            self.n = rest.len();
        }

        /// Zero-pad the pending partial block (the boundary between AAD and text).
        fn pad(&mut self) {
            if self.n > 0 {
                self.u.update_padded(&self.buf[..self.n]);
                self.n = 0;
            }
        }

        fn finish(mut self, lengths: [u8; 16]) -> [u8; 16] {
            self.pad();
            self.u.update(&[lengths.into()]);
            self.u.finalize().into()
        }
    }

    /// GCM or ChaCha20-Poly1305 as CTR keystream + universal hash, so text streams through `update`.
    enum Streaming {
        Gcm {
            ctr: Xform,
            mac: Mac<GHash>,
            ekj0: [u8; 16],
        },
        ChaPoly {
            c: chacha20::ChaCha20,
            mac: Mac<Poly1305>,
        },
    }

    struct Aead {
        s: Streaming,
        aad_len: u64,
        text_len: u64,
        started: bool,
    }

    impl Aead {
        fn aad(&mut self, d: &[u8]) -> bool {
            if self.started {
                return false;
            }
            self.aad_len += d.len() as u64;
            match &mut self.s {
                Streaming::Gcm { mac, .. } => mac.feed(d),
                Streaming::ChaPoly { mac, .. } => mac.feed(d),
            }
            true
        }

        fn update(&mut self, enc: bool, data: &[u8]) -> Vec<u8> {
            if !self.started {
                self.started = true;
                match &mut self.s {
                    Streaming::Gcm { mac, .. } => mac.pad(),
                    Streaming::ChaPoly { mac, .. } => mac.pad(),
                }
            }
            self.text_len += data.len() as u64;
            let mut out = data.to_vec();
            match &mut self.s {
                Streaming::Gcm { ctr, mac, .. } => {
                    if !enc {
                        mac.feed(data);
                    }
                    ctr(&mut out);
                    if enc {
                        mac.feed(&out);
                    }
                }
                Streaming::ChaPoly { c, mac } => {
                    if !enc {
                        mac.feed(data);
                    }
                    c.apply_keystream(&mut out);
                    if enc {
                        mac.feed(&out);
                    }
                }
            }
            out
        }

        fn tag(self) -> [u8; 16] {
            let mut lengths = [0u8; 16];
            match self.s {
                Streaming::Gcm { mac, ekj0, .. } => {
                    lengths[..8].copy_from_slice(&(self.aad_len * 8).to_be_bytes());
                    lengths[8..].copy_from_slice(&(self.text_len * 8).to_be_bytes());
                    let mut t = mac.finish(lengths);
                    for (a, b) in t.iter_mut().zip(ekj0) {
                        *a ^= b;
                    }
                    t
                }
                Streaming::ChaPoly { mut mac, .. } => {
                    mac.pad();
                    lengths[..8].copy_from_slice(&self.aad_len.to_le_bytes());
                    lengths[8..].copy_from_slice(&self.text_len.to_le_bytes());
                    mac.finish(lengths)
                }
            }
        }
    }

    fn gcm<C>(key: &[u8], iv: &[u8]) -> Result<Aead, OpError>
    where
        C: BlockCipher + BlockEncrypt + BlockSizeUser<BlockSize = U16> + KeyInit + Clone + 'static,
    {
        let c = C::new_from_slice(key).map_err(|_| invalid_keylen())?;
        let mut h = GenericArray::default();
        c.encrypt_block(&mut h);
        let j0: [u8; 16] = if iv.len() == 12 {
            let mut j = [0u8; 16];
            j[..12].copy_from_slice(iv);
            j[15] = 1;
            j
        } else {
            let mut g = GHash::new(&h);
            g.update_padded(iv);
            let mut lens = [0u8; 16];
            lens[8..].copy_from_slice(&(iv.len() as u64 * 8).to_be_bytes());
            g.update(&[lens.into()]);
            g.finalize().into()
        };
        let mut ekj0 = GenericArray::from(j0);
        c.encrypt_block(&mut ekj0);
        let mut first = j0;
        let ctr32 = u32::from_be_bytes([j0[12], j0[13], j0[14], j0[15]]).wrapping_add(1);
        first[12..].copy_from_slice(&ctr32.to_be_bytes());
        let ctr = ctr::Ctr32BE::<C>::new_from_slices(key, &first).map_err(|_| invalid_keylen())?;
        Ok(Aead {
            s: Streaming::Gcm {
                ctr: keystream(ctr),
                mac: Mac::new(GHash::new(&h)),
                ekj0: ekj0.into(),
            },
            aad_len: 0,
            text_len: 0,
            started: false,
        })
    }

    fn chapoly(key: &[u8], iv: &[u8]) -> Result<Aead, OpError> {
        let mut nonce = [0u8; 12];
        nonce[12 - iv.len()..].copy_from_slice(iv);
        let mut c =
            chacha20::ChaCha20::new_from_slices(key, &nonce).map_err(|_| invalid_keylen())?;
        let mut otk = [0u8; 64];
        c.apply_keystream(&mut otk);
        let mac = Mac::new(Poly1305::new_from_slice(&otk[..32]).map_err(|_| invalid_keylen())?);
        Ok(Aead {
            s: Streaming::ChaPoly { c, mac },
            aad_len: 0,
            text_len: 0,
            started: false,
        })
    }

    /// Run one-shot CCM over `buf` in place; `tag` receives (encrypt) or holds (decrypt) the tag.
    fn ccm_run<C>(
        enc: bool,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        buf: &mut [u8],
        tag: &mut [u8],
    ) -> bool
    where
        C: BlockCipher + BlockEncrypt + BlockSizeUser<BlockSize = U16> + KeyInit,
    {
        fn go<C, M, N>(
            enc: bool,
            key: &[u8],
            nonce: &[u8],
            aad: &[u8],
            buf: &mut [u8],
            tag: &mut [u8],
        ) -> bool
        where
            C: BlockCipher + BlockEncrypt + BlockSizeUser<BlockSize = U16> + KeyInit,
            M: cipher::ArrayLength<u8> + ccm::TagSize,
            N: cipher::ArrayLength<u8> + ccm::NonceSize,
        {
            use ccm::aead::AeadInPlace;
            let Ok(c) = <ccm::Ccm<C, M, N> as KeyInit>::new_from_slice(key) else {
                return false;
            };
            let n = GenericArray::from_slice(nonce);
            if enc {
                match c.encrypt_in_place_detached(n, aad, buf) {
                    Ok(t) => {
                        tag.copy_from_slice(&t);
                        true
                    }
                    Err(_) => false,
                }
            } else {
                c.decrypt_in_place_detached(n, aad, buf, GenericArray::from_slice(tag))
                    .is_ok()
            }
        }
        macro_rules! nonce {
            ($m:ty) => {
                match nonce.len() {
                    7 => go::<C, $m, cipher::consts::U7>(enc, key, nonce, aad, buf, tag),
                    8 => go::<C, $m, cipher::consts::U8>(enc, key, nonce, aad, buf, tag),
                    9 => go::<C, $m, cipher::consts::U9>(enc, key, nonce, aad, buf, tag),
                    10 => go::<C, $m, cipher::consts::U10>(enc, key, nonce, aad, buf, tag),
                    11 => go::<C, $m, cipher::consts::U11>(enc, key, nonce, aad, buf, tag),
                    12 => go::<C, $m, cipher::consts::U12>(enc, key, nonce, aad, buf, tag),
                    13 => go::<C, $m, cipher::consts::U13>(enc, key, nonce, aad, buf, tag),
                    _ => false,
                }
            };
        }
        match tag.len() {
            4 => nonce!(cipher::consts::U4),
            6 => nonce!(cipher::consts::U6),
            8 => nonce!(cipher::consts::U8),
            10 => nonce!(cipher::consts::U10),
            12 => nonce!(cipher::consts::U12),
            14 => nonce!(cipher::consts::U14),
            16 => nonce!(cipher::consts::U16),
            _ => false,
        }
    }

    /// OCB nonce/tag sizes this build carries: a 12-byte nonce with any tag, or any nonce of 6..=15
    /// bytes with a 16-byte tag.
    fn ocb_supported(nonce: usize, tag: usize) -> bool {
        (nonce == 12 && (1..=16).contains(&tag)) || ((6..=15).contains(&nonce) && tag == 16)
    }

    fn ocb_run<C>(
        enc: bool,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        buf: &mut [u8],
        tag: &mut [u8],
    ) -> bool
    where
        C: BlockCipher + BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16> + KeyInit,
    {
        use cipher::consts::*;
        use ocb3::aead::AeadInPlace;
        macro_rules! go {
            ($n:ty, $t:ty) => {{
                let Ok(c) = <ocb3::Ocb3<C, $n, $t> as KeyInit>::new_from_slice(key) else {
                    return false;
                };
                let n = GenericArray::from_slice(nonce);
                if enc {
                    match c.encrypt_in_place_detached(n, aad, buf) {
                        Ok(t) => {
                            tag.copy_from_slice(&t);
                            true
                        }
                        Err(_) => false,
                    }
                } else {
                    c.decrypt_in_place_detached(n, aad, buf, GenericArray::from_slice(tag))
                        .is_ok()
                }
            }};
        }
        match (nonce.len(), tag.len()) {
            (12, 1) => go!(U12, U1),
            (12, 2) => go!(U12, U2),
            (12, 3) => go!(U12, U3),
            (12, 4) => go!(U12, U4),
            (12, 5) => go!(U12, U5),
            (12, 6) => go!(U12, U6),
            (12, 7) => go!(U12, U7),
            (12, 8) => go!(U12, U8),
            (12, 9) => go!(U12, U9),
            (12, 10) => go!(U12, U10),
            (12, 11) => go!(U12, U11),
            (12, 12) => go!(U12, U12),
            (12, 13) => go!(U12, U13),
            (12, 14) => go!(U12, U14),
            (12, 15) => go!(U12, U15),
            (12, 16) => go!(U12, U16),
            (6, 16) => go!(U6, U16),
            (7, 16) => go!(U7, U16),
            (8, 16) => go!(U8, U16),
            (9, 16) => go!(U9, U16),
            (10, 16) => go!(U10, U16),
            (11, 16) => go!(U11, U16),
            (13, 16) => go!(U13, U16),
            (14, 16) => go!(U14, U16),
            (15, 16) => go!(U15, U16),
            _ => false,
        }
    }

    fn aead_dispatch(
        alg: Alg,
        ocb: bool,
        enc: bool,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        buf: &mut [u8],
        tag: &mut [u8],
    ) -> bool {
        match (alg, ocb) {
            (Alg::Aes128, false) => ccm_run::<aes::Aes128>(enc, key, nonce, aad, buf, tag),
            (Alg::Aes192, false) => ccm_run::<aes::Aes192>(enc, key, nonce, aad, buf, tag),
            (Alg::Aes256, false) => ccm_run::<aes::Aes256>(enc, key, nonce, aad, buf, tag),
            (Alg::Aes128, true) => ocb_run::<aes::Aes128>(enc, key, nonce, aad, buf, tag),
            (Alg::Aes192, true) => ocb_run::<aes::Aes192>(enc, key, nonce, aad, buf, tag),
            (Alg::Aes256, true) => ocb_run::<aes::Aes256>(enc, key, nonce, aad, buf, tag),
            _ => false,
        }
    }

    // ---- key wrap ----------------------------------------------------------------------------------

    const KW_IV: [u8; 8] = [0xA6; 8];
    const KWP_PREFIX: [u8; 4] = [0xA6, 0x59, 0x59, 0xA6];
    const DES3_WRAP_IV: [u8; 8] = [0x4a, 0xdd, 0xa2, 0x2c, 0x79, 0xe8, 0x21, 0x05];

    /// RFC 3394 `W` with an explicit initial value (OpenSSL lets callers choose it); `p` holds whole
    /// semiblocks.
    fn kw_wrap<C: BlockEncrypt + BlockSizeUser<BlockSize = U16>>(
        c: &C,
        iv: [u8; 8],
        p: &[u8],
    ) -> Vec<u8> {
        let n = p.len() / 8;
        let mut out = vec![0u8; 8 + p.len()];
        out[8..].copy_from_slice(p);
        let mut a = iv;
        let mut b = GenericArray::<u8, U16>::default();
        for j in 0..6 {
            for i in 1..=n {
                b[..8].copy_from_slice(&a);
                b[8..].copy_from_slice(&out[8 * i..8 * i + 8]);
                c.encrypt_block(&mut b);
                let t = ((n * j + i) as u64).to_be_bytes();
                for k in 0..8 {
                    a[k] = b[k] ^ t[k];
                }
                out[8 * i..8 * i + 8].copy_from_slice(&b[8..]);
            }
        }
        out[..8].copy_from_slice(&a);
        out
    }

    /// RFC 3394 `W^-1`: the recovered initial value and the plaintext semiblocks.
    fn kw_unwrap<C: BlockDecrypt + BlockSizeUser<BlockSize = U16>>(
        c: &C,
        ct: &[u8],
    ) -> ([u8; 8], Vec<u8>) {
        let n = ct.len() / 8 - 1;
        let mut a = [0u8; 8];
        a.copy_from_slice(&ct[..8]);
        let mut r = ct[8..].to_vec();
        let mut b = GenericArray::<u8, U16>::default();
        for j in (0..6).rev() {
            for i in (1..=n).rev() {
                let t = ((n * j + i) as u64).to_be_bytes();
                for k in 0..8 {
                    b[k] = a[k] ^ t[k];
                }
                b[8..].copy_from_slice(&r[8 * (i - 1)..8 * i]);
                c.decrypt_block(&mut b);
                a.copy_from_slice(&b[..8]);
                r[8 * (i - 1)..8 * i].copy_from_slice(&b[8..]);
            }
        }
        (a, r)
    }

    fn aes_wrap<C>(
        pad: bool,
        enc: bool,
        key: &[u8],
        iv: Option<&[u8]>,
        data: &[u8],
    ) -> Option<Vec<u8>>
    where
        C: BlockCipher + BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16> + KeyInit,
    {
        let c = C::new_from_slice(key).ok()?;
        let default_iv = iv.is_none_or(|v| if pad { v == KWP_PREFIX } else { v == KW_IV });
        if default_iv {
            let kek = aes_kw::Kek::<C>::try_from(key).ok()?;
            return match (pad, enc) {
                (false, true) if data.len() >= 16 => kek.wrap_vec(data).ok(),
                (false, false) if data.len() >= 24 => kek.unwrap_vec(data).ok(),
                (true, true) if !data.is_empty() => kek.wrap_with_padding_vec(data).ok(),
                (true, false) => kek.unwrap_with_padding_vec(data).ok(),
                _ => None,
            };
        }
        let iv = iv?;
        if !pad {
            let mut a = [0u8; 8];
            a.copy_from_slice(iv);
            if enc {
                if data.len() < 16 || data.len() % 8 != 0 {
                    return None;
                }
                return Some(kw_wrap(&c, a, data));
            }
            if data.len() < 24 || data.len() % 8 != 0 {
                return None;
            }
            let (got, p) = kw_unwrap(&c, data);
            return (got == a).then_some(p);
        }
        if enc {
            if data.is_empty() || data.len() > u32::MAX as usize {
                return None;
            }
            let mut aiv = [0u8; 8];
            aiv[..4].copy_from_slice(iv);
            aiv[4..].copy_from_slice(&(data.len() as u32).to_be_bytes());
            let mut p = data.to_vec();
            p.resize(data.len().div_ceil(8) * 8, 0);
            if p.len() == 8 {
                let mut b = GenericArray::<u8, U16>::default();
                b[..8].copy_from_slice(&aiv);
                b[8..].copy_from_slice(&p);
                c.encrypt_block(&mut b);
                return Some(b.to_vec());
            }
            return Some(kw_wrap(&c, aiv, &p));
        }
        if data.len() < 16 || data.len() % 8 != 0 {
            return None;
        }
        let (a, p) = if data.len() == 16 {
            let mut b = GenericArray::<u8, U16>::clone_from_slice(data);
            c.decrypt_block(&mut b);
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[..8]);
            (a, b[8..].to_vec())
        } else {
            kw_unwrap(&c, data)
        };
        if a[..4] != iv[..] {
            return None;
        }
        let mli = u32::from_be_bytes([a[4], a[5], a[6], a[7]]) as usize;
        if mli > p.len() || mli + 8 <= p.len() || p[mli..].iter().any(|&b| b != 0) {
            return None;
        }
        Some(p[..mli].to_vec())
    }

    fn tdes_cbc(enc: bool, key: &[u8], iv: &[u8], buf: &mut [u8]) -> Option<()> {
        let mut f = if enc {
            enc_blocks(cbc::Encryptor::<des::TdesEde3>::new_from_slices(key, iv).ok()?)
        } else {
            dec_blocks(cbc::Decryptor::<des::TdesEde3>::new_from_slices(key, iv).ok()?)
        };
        f(buf);
        Some(())
    }

    /// RFC 3217 Triple-DES key wrap (OpenSSL's `des3-wrap`, random IV).
    fn des3_wrap(enc: bool, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
        if enc {
            if data.is_empty() || data.len() % 8 != 0 {
                return None;
            }
            let mut iv = [0u8; 8];
            lumen_os::proc::entropy(&mut iv).ok()?;
            let mut out = Vec::with_capacity(data.len() + 16);
            out.extend_from_slice(&iv);
            out.extend_from_slice(data);
            out.extend_from_slice(&hash::digest(Algo::Sha1, data)[..8]);
            tdes_cbc(true, key, &iv, &mut out[8..])?;
            out.reverse();
            tdes_cbc(true, key, &DES3_WRAP_IV, &mut out)?;
            return Some(out);
        }
        if data.len() < 24 || data.len() % 8 != 0 {
            return None;
        }
        let mut t = data.to_vec();
        tdes_cbc(false, key, &DES3_WRAP_IV, &mut t)?;
        t.reverse();
        let (iv, rest) = t.split_at_mut(8);
        let iv = iv.to_vec();
        tdes_cbc(false, key, &iv, rest)?;
        let (p, icv) = rest.split_at(rest.len() - 8);
        let sum = hash::digest(Algo::Sha1, p);
        if !ct_eq(&sum[..8], icv) {
            return None;
        }
        Some(p.to_vec())
    }

    fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        use subtle::ConstantTimeEq;
        a.len() == b.len() && bool::from(a.ct_eq(b))
    }

    // ---- the cipher object -------------------------------------------------------------------------

    enum Engine {
        Block {
            f: Xform,
            bs: usize,
            pending: Vec<u8>,
            padding: bool,
        },
        Stream(Xform),
        Aead(Box<Aead>),
        /// CCM (one-shot in `update`) and OCB (buffered until `final`).
        OneShot {
            ocb: bool,
            key: Vec<u8>,
            nonce: Vec<u8>,
            aad: Vec<u8>,
            data: Vec<u8>,
            msg_len: Option<usize>,
            used: bool,
        },
        Wrap {
            key: Vec<u8>,
            iv: Option<Vec<u8>>,
        },
    }

    #[derive(PartialEq, Eq)]
    enum TagState {
        Unknown,
        Known,
    }

    /// The state of one `CipherBase`: `engine` is `None` once `final()` ran (OpenSSL's freed ctx).
    #[class(name = "CryptoCipher")]
    pub struct CryptoCipher {
        engine: Option<Engine>,
        spec: &'static Spec,
        enc: bool,
        tag_len: Option<usize>,
        tag_state: TagState,
        tag: Vec<u8>,
        max_message: usize,
        pending_auth_failed: bool,
    }

    #[methods]
    impl CryptoCipher {
        fn update(&mut self, data: &[u8]) -> Result<Vec<u8>, OpError> {
            let enc = self.enc;
            let spec = self.spec;
            if spec.mode == Mode::Ccm && data.len() > self.max_message {
                return Err(OpError::range_error("Invalid message length")
                    .with_code("ERR_CRYPTO_INVALID_MESSAGELEN"));
            }
            let Some(engine) = self.engine.as_mut() else {
                return Err(unsupported_update());
            };
            if data.len() + spec.block.max(1) > i32::MAX as usize {
                return Err(unsupported_update());
            }
            match engine {
                Engine::Block {
                    f,
                    bs,
                    pending,
                    padding,
                } => {
                    let bs = *bs;
                    let total = pending.len() + data.len();
                    let mut n = total / bs * bs;
                    if !enc && *padding && n == total && n > 0 {
                        n -= bs;
                    }
                    if n == 0 {
                        pending.extend_from_slice(data);
                        return Ok(Vec::new());
                    }
                    let mut out = Vec::with_capacity(n);
                    out.extend_from_slice(pending);
                    let used = n - pending.len();
                    out.extend_from_slice(&data[..used]);
                    pending.clear();
                    pending.extend_from_slice(&data[used..]);
                    f(&mut out);
                    Ok(out)
                }
                Engine::Stream(f) => {
                    let mut out = data.to_vec();
                    f(&mut out);
                    Ok(out)
                }
                Engine::Aead(a) => Ok(a.update(enc, data)),
                Engine::OneShot {
                    ocb: true,
                    data: buf,
                    ..
                } => {
                    buf.extend_from_slice(data);
                    Ok(Vec::new())
                }
                Engine::OneShot {
                    ocb: false,
                    key,
                    nonce,
                    aad,
                    msg_len,
                    used,
                    ..
                } => {
                    if data.is_empty() && !*used {
                        return Ok(Vec::new());
                    }
                    let fits = !*used && msg_len.is_none_or(|m| m == data.len());
                    *used = true;
                    if !enc && (self.tag_state != TagState::Known || !fits) {
                        self.pending_auth_failed = true;
                        return Ok(Vec::new());
                    }
                    if !fits {
                        return Err(unsupported_update());
                    }
                    let mut out = data.to_vec();
                    let tag_len = self.tag_len.unwrap_or(16);
                    if enc {
                        self.tag = vec![0; tag_len];
                    }
                    if aead_dispatch(
                        spec.alg,
                        false,
                        enc,
                        key,
                        nonce,
                        aad,
                        &mut out,
                        &mut self.tag,
                    ) {
                        Ok(out)
                    } else if enc {
                        Err(unsupported_update())
                    } else {
                        self.pending_auth_failed = true;
                        Ok(Vec::new())
                    }
                }
                Engine::Wrap { key, iv } => {
                    if data.is_empty() {
                        return Ok(Vec::new());
                    }
                    let out = match (spec.alg, spec.mode) {
                        (_, Mode::Des3Wrap) => des3_wrap(enc, key, data),
                        (Alg::Aes128, m) => aes_wrap::<aes::Aes128>(
                            m == Mode::WrapPad,
                            enc,
                            key,
                            iv.as_deref(),
                            data,
                        ),
                        (Alg::Aes192, m) => aes_wrap::<aes::Aes192>(
                            m == Mode::WrapPad,
                            enc,
                            key,
                            iv.as_deref(),
                            data,
                        ),
                        (Alg::Aes256, m) => aes_wrap::<aes::Aes256>(
                            m == Mode::WrapPad,
                            enc,
                            key,
                            iv.as_deref(),
                            data,
                        ),
                        _ => None,
                    };
                    out.ok_or_else(unsupported_update)
                }
            }
        }

        #[method(name = "final")]
        fn finish(&mut self) -> Result<Vec<u8>, OpError> {
            let Some(engine) = self.engine.take() else {
                return Err(OpError::error("Invalid state for operation final")
                    .with_code("ERR_CRYPTO_INVALID_STATE"));
            };
            let enc = self.enc;
            match engine {
                Engine::Block {
                    mut f,
                    bs,
                    mut pending,
                    padding,
                } => {
                    if enc {
                        if padding {
                            let p = bs - pending.len();
                            pending.resize(bs, p as u8);
                        } else if !pending.is_empty() {
                            return Err(wrong_final_block_length());
                        }
                        f(&mut pending);
                        return Ok(pending);
                    }
                    if !padding {
                        return if pending.is_empty() {
                            Ok(pending)
                        } else {
                            Err(wrong_final_block_length())
                        };
                    }
                    if pending.len() != bs {
                        return Err(wrong_final_block_length());
                    }
                    f(&mut pending);
                    let p = pending[bs - 1] as usize;
                    if p == 0 || p > bs || pending[bs - p..].iter().any(|&b| b as usize != p) {
                        return Err(bad_decrypt());
                    }
                    pending.truncate(bs - p);
                    Ok(pending)
                }
                Engine::Stream(_) | Engine::Wrap { .. } => Ok(Vec::new()),
                Engine::Aead(a) => {
                    let full = a.tag();
                    if enc {
                        let n = *self.tag_len.get_or_insert(16);
                        self.tag = full[..n].to_vec();
                        return Ok(Vec::new());
                    }
                    if self.tag_state != TagState::Known
                        || !ct_eq(&full[..self.tag.len()], &self.tag)
                    {
                        return Err(auth_failed());
                    }
                    Ok(Vec::new())
                }
                Engine::OneShot {
                    ocb: false, used, ..
                } => {
                    if enc {
                        if !used {
                            return Err(ossl("1C800077", "tag not set", "ERR_OSSL_TAG_NOT_SET"));
                        }
                        return Ok(Vec::new());
                    }
                    if self.pending_auth_failed {
                        return Err(auth_failed());
                    }
                    Ok(Vec::new())
                }
                Engine::OneShot {
                    ocb: true,
                    key,
                    nonce,
                    aad,
                    mut data,
                    ..
                } => {
                    let tag_len = self.tag_len.unwrap_or(16);
                    if enc {
                        self.tag = vec![0; tag_len];
                    } else if self.tag_state != TagState::Known {
                        return Err(auth_failed());
                    }
                    if !aead_dispatch(
                        self.spec.alg,
                        true,
                        enc,
                        &key,
                        &nonce,
                        &aad,
                        &mut data,
                        &mut self.tag,
                    ) {
                        return Err(auth_failed());
                    }
                    Ok(data)
                }
            }
        }

        /// `setAutoPadding`: false once finalized.
        fn set_padding(&mut self, on: bool) -> bool {
            match self.engine.as_mut() {
                None => false,
                Some(Engine::Block { padding, .. }) => {
                    *padding = on;
                    true
                }
                Some(_) => true,
            }
        }

        /// `getAuthTag`: the tag after an encrypting `final()`, otherwise `null`.
        fn auth_tag(&self) -> Option<Vec<u8>> {
            if self.engine.is_some()
                || !self.enc
                || self.tag_len.is_none()
                || !self.spec.mode.is_aead()
            {
                return None;
            }
            Some(self.tag.clone())
        }

        /// `setAuthTag`: false in a state that does not take a tag; throws on an invalid length.
        fn set_auth_tag(&mut self, tag: &[u8]) -> Result<bool, OpError> {
            if self.engine.is_none()
                || !self.spec.mode.is_aead()
                || self.enc
                || self.tag_state != TagState::Unknown
            {
                return Ok(false);
            }
            let n = tag.len();
            let valid = if self.spec.mode == Mode::Gcm {
                self.tag_len.is_none_or(|l| l == n) && valid_gcm_tag(n)
            } else {
                self.tag_len == Some(n)
            };
            if !valid {
                return Err(invalid_tag(format!(
                    "Invalid authentication tag length: {n}"
                )));
            }
            self.tag_len = Some(n);
            self.tag_state = TagState::Known;
            self.tag = tag.to_vec();
            Ok(true)
        }

        /// `setAAD(data, plaintextLength)`; `plaintext_len` < 0 when not given.
        fn set_aad(&mut self, data: &[u8], plaintext_len: f64) -> Result<bool, OpError> {
            let mode = self.spec.mode;
            let max = self.max_message;
            let Some(engine) = self.engine.as_mut() else {
                return Ok(false);
            };
            match engine {
                Engine::Aead(a) => Ok(a.aad(data)),
                Engine::OneShot {
                    ocb: true,
                    aad,
                    data: buf,
                    ..
                } => {
                    if !buf.is_empty() {
                        return Ok(false);
                    }
                    aad.extend_from_slice(data);
                    Ok(true)
                }
                Engine::OneShot {
                    ocb: false,
                    aad,
                    msg_len,
                    used,
                    ..
                } if mode == Mode::Ccm => {
                    if plaintext_len < 0.0 {
                        return Err(OpError::type_error(
                            "options.plaintextLength required for CCM mode with AAD",
                        )
                        .with_code("ERR_MISSING_ARGS"));
                    }
                    if plaintext_len > max as f64 {
                        return Err(OpError::range_error("Invalid message length")
                            .with_code("ERR_CRYPTO_INVALID_MESSAGELEN"));
                    }
                    if *used {
                        return Ok(false);
                    }
                    *msg_len = Some(plaintext_len as usize);
                    aad.extend_from_slice(data);
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
    }

    fn valid_gcm_tag(n: usize) -> bool {
        n == 4 || n == 8 || (12..=16).contains(&n)
    }

    /// `CipherBase::CommonInit` (+ `InitAuthenticated`): validate the AEAD parameters and key, then
    /// build the engine.
    fn common_init(
        spec: &'static Spec,
        name: &str,
        enc: bool,
        key: &[u8],
        iv: &[u8],
        tag_len: f64,
    ) -> Result<CryptoCipher, OpError> {
        let mut tag = if tag_len >= 0.0 {
            Some(tag_len as usize)
        } else {
            None
        };
        let mut max_message = i32::MAX as usize;
        if spec.mode.is_aead() {
            let iv_ok = match spec.mode {
                Mode::Gcm => !iv.is_empty(),
                Mode::Ccm => (7..=13).contains(&iv.len()),
                Mode::Ocb => (1..=15).contains(&iv.len()),
                _ => (1..=12).contains(&iv.len()),
            };
            if !iv_ok {
                return Err(invalid_iv());
            }
            if spec.mode == Mode::Gcm {
                if let Some(t) = tag {
                    if !valid_gcm_tag(t) {
                        return Err(invalid_tag(format!(
                            "Invalid authentication tag length: {}",
                            tag_len as u64
                        )));
                    }
                }
            } else {
                let t = match tag {
                    Some(t) => t,
                    None if spec.mode == Mode::ChaPoly => 16,
                    None => return Err(invalid_tag(format!("authTagLength required for {name}"))),
                };
                let ok = match spec.mode {
                    Mode::Ccm => t % 2 == 0 && (4..=16).contains(&t),
                    Mode::Ocb => (1..=16).contains(&t),
                    _ => (1..=16).contains(&t),
                };
                if !ok {
                    return Err(invalid_tag(format!(
                        "Invalid authentication tag length: {}",
                        tag_len as u64
                    )));
                }
                if spec.mode == Mode::Ocb && !ocb_supported(iv.len(), t) {
                    return Err(invalid_iv());
                }
                tag = Some(t);
                if spec.mode == Mode::Ccm {
                    max_message = match iv.len() {
                        12 => 16_777_215,
                        13 => 65_535,
                        _ => i32::MAX as usize,
                    };
                }
            }
        }
        if key.len() != spec.key {
            return Err(invalid_keylen());
        }
        let engine = build(spec, enc, key, iv)?;
        Ok(CryptoCipher {
            engine: Some(engine),
            spec,
            enc,
            tag_len: tag,
            tag_state: TagState::Unknown,
            tag: Vec::new(),
            max_message,
            pending_auth_failed: false,
        })
    }

    fn build(spec: &Spec, enc: bool, key: &[u8], iv: &[u8]) -> Result<Engine, OpError> {
        fn aes<C>(spec: &Spec, enc: bool, key: &[u8], iv: &[u8]) -> Result<Engine, OpError>
        where
            C: BlockCipher
                + BlockEncrypt
                + BlockDecrypt
                + BlockSizeUser<BlockSize = U16>
                + KeyInit
                + Clone
                + 'static,
        {
            Ok(match spec.mode {
                Mode::Ctr => Engine::Stream(keystream(
                    ctr::Ctr128BE::<C>::new_from_slices(key, iv).map_err(|_| invalid_keylen())?,
                )),
                Mode::Gcm => Engine::Aead(Box::new(gcm::<C>(key, iv)?)),
                Mode::Ccm | Mode::Ocb => Engine::OneShot {
                    ocb: spec.mode == Mode::Ocb,
                    key: key.to_vec(),
                    nonce: iv.to_vec(),
                    aad: Vec::new(),
                    data: Vec::new(),
                    msg_len: None,
                    used: false,
                },
                _ => generic::<C>(spec, enc, key, iv)?,
            })
        }
        fn generic<C>(spec: &Spec, enc: bool, key: &[u8], iv: &[u8]) -> Result<Engine, OpError>
        where
            C: BlockCipher + BlockEncrypt + BlockDecrypt + KeyInit + Clone + 'static,
            C::BlockSize: IsLess<U256>,
            Le<C::BlockSize, U256>: NonZero,
        {
            let (f, bs) = generic_mode::<C>(spec.mode, enc, key, iv)?;
            Ok(if bs > 1 {
                Engine::Block {
                    f,
                    bs,
                    pending: Vec::new(),
                    padding: true,
                }
            } else {
                Engine::Stream(f)
            })
        }
        match spec.mode {
            Mode::Wrap | Mode::WrapPad | Mode::Des3Wrap => {
                return Ok(Engine::Wrap {
                    key: key.to_vec(),
                    iv: (!iv.is_empty()).then(|| iv.to_vec()),
                });
            }
            Mode::ChaPoly => return Ok(Engine::Aead(Box::new(chapoly(key, iv)?))),
            Mode::Stream => {
                let counter = u32::from_le_bytes([iv[0], iv[1], iv[2], iv[3]]);
                let mut c = chacha20::ChaCha20::new_from_slices(key, &iv[4..])
                    .map_err(|_| invalid_keylen())?;
                c.seek(counter as u64 * 64);
                return Ok(Engine::Stream(keystream(c)));
            }
            _ => {}
        }
        match spec.alg {
            Alg::Aes128 => aes::<aes::Aes128>(spec, enc, key, iv),
            Alg::Aes192 => aes::<aes::Aes192>(spec, enc, key, iv),
            Alg::Aes256 => aes::<aes::Aes256>(spec, enc, key, iv),
            Alg::Tdes2 => generic::<des::TdesEde2>(spec, enc, key, iv),
            Alg::Tdes3 => generic::<des::TdesEde3>(spec, enc, key, iv),
            Alg::ChaCha => {
                Err(OpError::error("Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER"))
            }
        }
    }

    fn unknown_cipher() -> OpError {
        OpError::error("Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER")
    }

    /// `CipherBase#initiv`: key, IV (`null` for none) and `authTagLength` (-1 for none).
    #[op(name = "cipherNew")]
    fn cipher_new(
        name: &str,
        enc: bool,
        key: &[u8],
        iv: Option<&[u8]>,
        tag_len: f64,
    ) -> Result<CryptoCipher, OpError> {
        let spec = lookup(name).ok_or_else(unknown_cipher)?;
        let iv = iv.unwrap_or(&[]);
        if iv.is_empty() && spec.iv != 0 {
            return Err(invalid_iv());
        }
        if !spec.mode.is_aead() && !iv.is_empty() && iv.len() != spec.iv {
            return Err(invalid_iv());
        }
        if spec.mode == Mode::ChaPoly && iv.len() > 12 {
            return Err(invalid_iv());
        }
        common_init(spec, name, enc, key, iv, tag_len)
    }

    /// `CipherBase#init`: the key and IV come from `EVP_BytesToKey(md5, no salt, 1 round)` over the
    /// password.
    #[op(name = "cipherInit")]
    fn cipher_init(
        name: &str,
        enc: bool,
        password: &[u8],
        tag_len: f64,
    ) -> Result<CryptoCipher, OpError> {
        let spec = lookup(name).ok_or_else(unknown_cipher)?;
        let need = spec.key + spec.iv;
        let out = bytes_to_key(password, &[], need);
        common_init(
            spec,
            name,
            enc,
            &out[..spec.key],
            &out[spec.key..need],
            tag_len,
        )
    }

    /// OpenSSL's `EVP_BytesToKey(cipher, md5, salt, pass, count = 1)`: `len` bytes of chained MD5.
    pub(crate) fn bytes_to_key(pass: &[u8], salt: &[u8], len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 16);
        let mut prev: Vec<u8> = Vec::new();
        while out.len() < len {
            let mut h = hash::Hasher::new(Algo::Md5);
            h.update(&prev);
            h.update(pass);
            h.update(salt);
            prev = h.finish();
            out.extend_from_slice(&prev);
        }
        out.truncate(len);
        out
    }

    /// The mode label of a known cipher name (`null` when unknown).
    #[op(name = "cipherMode")]
    fn cipher_mode(name: &str) -> Option<&'static str> {
        lookup(name).map(|s| s.mode.label())
    }

    /// WebCrypto AES-CTR: the low `length` bits of `counter` count blocks and wrap to zero without
    /// carrying into the nonce bits.
    #[op(name = "aesCtr")]
    fn aes_ctr(key: &[u8], counter: &[u8], length: u32, data: &[u8]) -> Result<Vec<u8>, OpError> {
        fn run<C>(key: &[u8], iv: &[u8], buf: &mut [u8]) -> Result<(), OpError>
        where
            C: BlockCipher + BlockEncrypt + BlockSizeUser<BlockSize = U16> + KeyInit,
        {
            let mut c =
                ctr::Ctr128BE::<C>::new_from_slices(key, iv).map_err(|_| invalid_keylen())?;
            c.apply_keystream(buf);
            Ok(())
        }
        let f: fn(&[u8], &[u8], &mut [u8]) -> Result<(), OpError> = match key.len() {
            16 => run::<aes::Aes128>,
            24 => run::<aes::Aes192>,
            32 => run::<aes::Aes256>,
            _ => return Err(invalid_keylen()),
        };
        if counter.len() != 16 || length == 0 || length > 128 {
            return Err(
                OpError::type_error("Invalid counter").with_code("ERR_CRYPTO_INVALID_COUNTER")
            );
        }
        let mut out = data.to_vec();
        let ctr = u128::from_be_bytes(counter.try_into().unwrap());
        let mask = if length == 128 {
            u128::MAX
        } else {
            (1u128 << length) - 1
        };
        let blocks = data.len().div_ceil(16) as u128;
        let current = ctr & mask;
        let room = mask - current;
        if blocks == 0 || blocks - 1 <= room {
            f(key, counter, &mut out)?;
            return Ok(out);
        }
        if length < 128 && blocks - 1 > mask {
            return Err(OpError::error("Cipher job failed"));
        }
        let first = ((room + 1) * 16) as usize;
        let (a, b) = out.split_at_mut(first);
        f(key, counter, a)?;
        f(key, &(ctr & !mask).to_be_bytes(), b)?;
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn hex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }

        /// Encrypts `pt` in `chunk`-sized `update`s with the AAD fed in `aad_chunk`-sized pieces;
        /// returns the ciphertext and the tag.
        fn seal(
            name: &str,
            key: &[u8],
            iv: &[u8],
            aad: &[u8],
            pt: &[u8],
            tag_len: f64,
            chunk: usize,
            aad_chunk: usize,
        ) -> (Vec<u8>, Vec<u8>) {
            let mut c = cipher_new(name, true, key, Some(iv), tag_len).unwrap();
            for piece in aad.chunks(aad_chunk.max(1)) {
                assert!(c.set_aad(piece, -1.0).unwrap());
            }
            let mut ct = Vec::new();
            for piece in pt.chunks(chunk.max(1)) {
                ct.extend(c.update(piece).unwrap());
            }
            ct.extend(c.finish().unwrap());
            (ct, c.auth_tag().unwrap())
        }

        fn open(
            name: &str,
            key: &[u8],
            iv: &[u8],
            aad: &[u8],
            ct: &[u8],
            tag: &[u8],
            tag_len: f64,
        ) -> Result<Vec<u8>, OpError> {
            let mut c = cipher_new(name, false, key, Some(iv), tag_len)?;
            if !tag.is_empty() {
                c.set_auth_tag(tag)?;
            }
            if !aad.is_empty() {
                c.set_aad(aad, -1.0)?;
            }
            let mut pt = c.update(ct)?;
            pt.extend(c.finish()?);
            Ok(pt)
        }

        struct Gcm {
            name: &'static str,
            key: &'static str,
            iv: &'static str,
            aad: &'static str,
            pt: &'static str,
            ct: &'static str,
            tag: &'static str,
        }

        const PT64: &str = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255";
        const PT60: &str = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39";
        const AAD: &str = "feedfacedeadbeeffeedfacedeadbeefabaddad2";
        const K128: &str = "feffe9928665731c6d6a8f9467308308";

        // The test cases of "The Galois/Counter Mode of Operation" (McGrew & Viega), as used by NIST SP 800-38D.
        fn vectors() -> Vec<Gcm> {
            vec![
                Gcm {
                    name: "aes-128-gcm",
                    key: "00000000000000000000000000000000",
                    iv: "000000000000000000000000",
                    aad: "",
                    pt: "",
                    ct: "",
                    tag: "58e2fccefa7e3061367f1d57a4e7455a",
                },
                Gcm {
                    name: "aes-128-gcm",
                    key: "00000000000000000000000000000000",
                    iv: "000000000000000000000000",
                    aad: "",
                    pt: "00000000000000000000000000000000",
                    ct: "0388dace60b6a392f328c2b971b2fe78",
                    tag: "ab6e47d42cec13bdf53a67b21257bddf",
                },
                Gcm {
                    name: "aes-128-gcm",
                    key: K128,
                    iv: "cafebabefacedbaddecaf888",
                    aad: "",
                    pt: PT64,
                    ct: "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985",
                    tag: "4d5c2af327cd64a62cf35abd2ba6fab4",
                },
                Gcm {
                    name: "aes-128-gcm",
                    key: K128,
                    iv: "cafebabefacedbaddecaf888",
                    aad: AAD,
                    pt: PT60,
                    ct: "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091",
                    tag: "5bc94fbc3221a5db94fae95ae7121a47",
                },
                Gcm {
                    name: "aes-128-gcm",
                    key: K128,
                    iv: "cafebabefacedbad",
                    aad: AAD,
                    pt: PT60,
                    ct: "61353b4c2806934a777ff51fa22a4755699b2a714fcdc6f83766e5f97b6c742373806900e49f24b22b097544d4896b424989b5e1ebac0f07c23f4598",
                    tag: "3612d2e79e3b0785561be14aaca2fccb",
                },
                Gcm {
                    name: "aes-128-gcm",
                    key: K128,
                    iv: "9313225df88406e555909c5aff5269aa6a7a9538534f7da1e4c303d2a318a728c3c0c95156809539fcf0e2429a6b525416aedbf5a0de6a57a637b39b",
                    aad: AAD,
                    pt: PT60,
                    ct: "8ce24998625615b603a033aca13fb894be9112a5c3a211a8ba262a3cca7e2ca701e4a9a4fba43c90ccdcb281d48c7c6fd62875d2aca417034c34aee5",
                    tag: "619cc5aefffe0bfa462af43c1699d050",
                },
                Gcm {
                    name: "aes-256-gcm",
                    key: "0000000000000000000000000000000000000000000000000000000000000000",
                    iv: "000000000000000000000000",
                    aad: "",
                    pt: "",
                    ct: "",
                    tag: "530f8afbc74536b9a963b4f1c4cb738b",
                },
                Gcm {
                    name: "aes-256-gcm",
                    key: "feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308",
                    iv: "cafebabefacedbaddecaf888",
                    aad: "",
                    pt: PT64,
                    ct: "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662898015ad",
                    tag: "b094dac5d93471bdec1a502270e3cc6c",
                },
            ]
        }

        #[test]
        fn gcm_matches_nist_vectors() {
            for (i, v) in vectors().iter().enumerate() {
                let (key, iv, aad, pt) = (hex(v.key), hex(v.iv), hex(v.aad), hex(v.pt));
                let (ct, tag) = seal(v.name, &key, &iv, &aad, &pt, -1.0, usize::MAX, usize::MAX);
                assert_eq!(ct, hex(v.ct), "ciphertext {i}");
                assert_eq!(tag, hex(v.tag), "tag {i}");
                assert_eq!(
                    open(v.name, &key, &iv, &aad, &ct, &tag, -1.0).unwrap(),
                    pt,
                    "decrypt {i}"
                );
            }
        }

        #[test]
        fn gcm_streaming_is_independent_of_chunking() {
            for v in vectors().iter().filter(|v| !v.aad.is_empty()) {
                let (key, iv, aad, pt) = (hex(v.key), hex(v.iv), hex(v.aad), hex(v.pt));
                for (chunk, aad_chunk) in [(1, 1), (3, 5), (15, 16), (16, 17), (17, 4), (64, 64)] {
                    let (ct, tag) = seal(v.name, &key, &iv, &aad, &pt, -1.0, chunk, aad_chunk);
                    assert_eq!(ct, hex(v.ct), "chunk {chunk}/{aad_chunk}");
                    assert_eq!(tag, hex(v.tag), "chunk {chunk}/{aad_chunk}");
                }
            }
        }

        #[test]
        fn gcm_truncated_tags() {
            let v = &vectors()[3];
            let (key, iv, aad, pt) = (hex(v.key), hex(v.iv), hex(v.aad), hex(v.pt));
            let full = hex(v.tag);
            for n in [4usize, 8, 12, 13, 14, 15, 16] {
                let (ct, tag) = seal(
                    v.name,
                    &key,
                    &iv,
                    &aad,
                    &pt,
                    n as f64,
                    usize::MAX,
                    usize::MAX,
                );
                assert_eq!(tag, full[..n], "length {n}");
                assert_eq!(
                    open(v.name, &key, &iv, &aad, &ct, &tag, n as f64).unwrap(),
                    pt
                );
                assert_eq!(open(v.name, &key, &iv, &aad, &ct, &tag, -1.0).unwrap(), pt);
            }
            for n in [0usize, 1, 2, 3, 5, 6, 7, 9, 10, 11, 17] {
                assert!(
                    cipher_new(v.name, true, &key, Some(&iv), n as f64).is_err(),
                    "authTagLength {n}"
                );
                let mut c = cipher_new(v.name, false, &key, Some(&iv), -1.0).unwrap();
                assert!(
                    c.set_auth_tag(&vec![0u8; n]).is_err(),
                    "setAuthTag length {n}"
                );
            }
            let mut c = cipher_new(v.name, false, &key, Some(&iv), 8.0).unwrap();
            assert!(c.set_auth_tag(&full[..12]).is_err());
        }

        #[test]
        fn gcm_rejects_any_tampering() {
            let v = &vectors()[3];
            let (key, iv, aad, pt) = (hex(v.key), hex(v.iv), hex(v.aad), hex(v.pt));
            let (ct, tag) = seal(v.name, &key, &iv, &aad, &pt, -1.0, usize::MAX, usize::MAX);
            for i in 0..tag.len() {
                let mut bad = tag.clone();
                bad[i] ^= 1;
                let e = open(v.name, &key, &iv, &aad, &ct, &bad, -1.0)
                    .unwrap_err()
                    .to_string();
                assert!(e.contains("authenticate"), "{e}");
            }
            for n in [4usize, 12] {
                let mut bad = tag[..n].to_vec();
                bad[n - 1] ^= 0x80;
                assert!(open(v.name, &key, &iv, &aad, &ct, &bad, n as f64).is_err());
            }
            let mut bad_ct = ct.clone();
            bad_ct[10] ^= 1;
            assert!(open(v.name, &key, &iv, &aad, &bad_ct, &tag, -1.0).is_err());
            let mut bad_aad = aad.clone();
            bad_aad[0] ^= 1;
            assert!(open(v.name, &key, &iv, &bad_aad, &ct, &tag, -1.0).is_err());
            assert!(open(v.name, &key, &iv, &aad, &ct, &[], -1.0).is_err());
        }

        const CHAPOLY_KEY: &str =
            "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f";
        const CHAPOLY_IV: &str = "070000004041424344454647";
        const CHAPOLY_AAD: &str = "50515253c0c1c2c3c4c5c6c7";
        const CHAPOLY_PT: &[u8] = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        const CHAPOLY_CT: &str = "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116";
        const CHAPOLY_TAG: &str = "1ae10b594f09e26a7e902ecbd0600691";

        #[test]
        fn chacha20_poly1305_matches_rfc_8439() {
            let (key, iv, aad) = (hex(CHAPOLY_KEY), hex(CHAPOLY_IV), hex(CHAPOLY_AAD));
            let (ct, tag) = seal(
                "chacha20-poly1305",
                &key,
                &iv,
                &aad,
                CHAPOLY_PT,
                16.0,
                usize::MAX,
                usize::MAX,
            );
            assert_eq!(ct, hex(CHAPOLY_CT));
            assert_eq!(tag, hex(CHAPOLY_TAG));
            assert_eq!(
                open("chacha20-poly1305", &key, &iv, &aad, &ct, &tag, 16.0).unwrap(),
                CHAPOLY_PT
            );
        }

        #[test]
        fn chacha20_poly1305_streaming_and_truncation() {
            let (key, iv, aad) = (hex(CHAPOLY_KEY), hex(CHAPOLY_IV), hex(CHAPOLY_AAD));
            for (chunk, aad_chunk) in [(1, 1), (7, 3), (16, 16), (63, 13), (64, 64)] {
                let (ct, tag) = seal(
                    "chacha20-poly1305",
                    &key,
                    &iv,
                    &aad,
                    CHAPOLY_PT,
                    16.0,
                    chunk,
                    aad_chunk,
                );
                assert_eq!(ct, hex(CHAPOLY_CT), "chunk {chunk}/{aad_chunk}");
                assert_eq!(tag, hex(CHAPOLY_TAG), "chunk {chunk}/{aad_chunk}");
            }
            let full = hex(CHAPOLY_TAG);
            for n in 1..=16usize {
                let (ct, tag) = seal(
                    "chacha20-poly1305",
                    &key,
                    &iv,
                    &aad,
                    CHAPOLY_PT,
                    n as f64,
                    usize::MAX,
                    usize::MAX,
                );
                assert_eq!(tag, full[..n]);
                assert_eq!(
                    open("chacha20-poly1305", &key, &iv, &aad, &ct, &tag, n as f64).unwrap(),
                    CHAPOLY_PT
                );
            }
        }

        #[test]
        fn chacha20_poly1305_rejects_any_tampering() {
            let (key, iv, aad, ct, tag) = (
                hex(CHAPOLY_KEY),
                hex(CHAPOLY_IV),
                hex(CHAPOLY_AAD),
                hex(CHAPOLY_CT),
                hex(CHAPOLY_TAG),
            );
            for i in 0..tag.len() {
                let mut bad = tag.clone();
                bad[i] ^= 1;
                let e = open("chacha20-poly1305", &key, &iv, &aad, &ct, &bad, 16.0)
                    .unwrap_err()
                    .to_string();
                assert!(e.contains("authenticate"), "{e}");
            }
            let mut bad_ct = ct.clone();
            bad_ct[0] ^= 1;
            assert!(open("chacha20-poly1305", &key, &iv, &aad, &bad_ct, &tag, 16.0).is_err());
            let mut bad_aad = aad.clone();
            bad_aad[11] ^= 1;
            assert!(open("chacha20-poly1305", &key, &iv, &bad_aad, &ct, &tag, 16.0).is_err());
            assert!(open("chacha20-poly1305", &key, &iv, &aad, &ct, &[], 16.0).is_err());
        }

        #[test]
        fn tag_comparison_is_length_and_content_exact() {
            assert!(ct_eq(b"abcd", b"abcd"));
            assert!(!ct_eq(b"abcd", b"abce"));
            assert!(!ct_eq(b"abcd", b"abc"));
            assert!(ct_eq(b"", b""));
        }
    }
}
