//! Message digests, HMAC and the password-based KDFs behind `node:crypto` and `crypto.subtle`,
//! all from the RustCrypto crates (`digest` family, `hmac`, `pbkdf2`, `hkdf`, `scrypt`).

use digest::{Digest, ExtendableOutput, Update};
use hmac::{Mac, SimpleHmac};

/// A digest algorithm `createHash` knows by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
    Sha3_224,
    Sha3_256,
    Sha3_384,
    Sha3_512,
    Shake128,
    Shake256,
    Ripemd160,
    Blake2b512,
    Blake2s256,
    Sm3,
    Md5Sha1,
}

impl Algo {
    /// OpenSSL's names and aliases, case-insensitive (`sha256`, `SHA-256`, `RSA-SHA256`, ...).
    pub fn from_name(name: &str) -> Option<Algo> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "md5" | "rsa-md5" | "ssl3-md5" | "md5withrsaencryption" => Algo::Md5,
            "sha1" | "sha-1" | "rsa-sha1" | "rsa-sha1-2" | "ssl3-sha1" | "sha1withrsaencryption" => Algo::Sha1,
            "sha224" | "sha-224" | "rsa-sha224" | "sha224withrsaencryption" => Algo::Sha224,
            "sha256" | "sha-256" | "rsa-sha256" | "sha256withrsaencryption" => Algo::Sha256,
            "sha384" | "sha-384" | "rsa-sha384" | "sha384withrsaencryption" => Algo::Sha384,
            "sha512" | "sha-512" | "rsa-sha512" | "sha512withrsaencryption" => Algo::Sha512,
            "sha512-224" | "sha-512/224" | "rsa-sha512/224" | "sha512-224withrsaencryption" => Algo::Sha512_224,
            "sha512-256" | "sha-512/256" | "rsa-sha512/256" | "sha512-256withrsaencryption" => Algo::Sha512_256,
            "sha3-224" | "rsa-sha3-224" | "id-rsassa-pkcs1-v1_5-with-sha3-224" => Algo::Sha3_224,
            "sha3-256" | "rsa-sha3-256" | "id-rsassa-pkcs1-v1_5-with-sha3-256" => Algo::Sha3_256,
            "sha3-384" | "rsa-sha3-384" | "id-rsassa-pkcs1-v1_5-with-sha3-384" => Algo::Sha3_384,
            "sha3-512" | "rsa-sha3-512" | "id-rsassa-pkcs1-v1_5-with-sha3-512" => Algo::Sha3_512,
            "shake128" | "shake-128" => Algo::Shake128,
            "shake256" | "shake-256" => Algo::Shake256,
            "ripemd160" | "ripemd" | "rmd160" | "rsa-ripemd160" | "ripemd160withrsa" => Algo::Ripemd160,
            "blake2b512" => Algo::Blake2b512,
            "blake2s256" => Algo::Blake2s256,
            "sm3" | "rsa-sm3" | "sm3withrsaencryption" => Algo::Sm3,
            "md5-sha1" => Algo::Md5Sha1,
            _ => return None,
        })
    }

    pub fn out_len(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 | Algo::Ripemd160 => 20,
            Algo::Sha224 | Algo::Sha512_224 | Algo::Sha3_224 => 28,
            Algo::Sha256 | Algo::Sha512_256 | Algo::Sha3_256 | Algo::Shake256 | Algo::Sm3 | Algo::Blake2s256 => 32,
            Algo::Sha384 | Algo::Sha3_384 => 48,
            Algo::Sha512 | Algo::Sha3_512 | Algo::Blake2b512 => 64,
            Algo::Shake128 => 16,
            Algo::Md5Sha1 => 36,
        }
    }

    pub fn is_xof(self) -> bool {
        matches!(self, Algo::Shake128 | Algo::Shake256)
    }

    /// Compression block length in bytes (the HMAC block size).
    pub fn block_len(self) -> usize {
        match self {
            Algo::Md5 | Algo::Sha1 | Algo::Sha224 | Algo::Sha256 | Algo::Ripemd160 | Algo::Sm3 | Algo::Md5Sha1 => 64,
            Algo::Sha3_224 => 144,
            Algo::Sha3_256 | Algo::Shake256 => 136,
            Algo::Sha3_384 => 104,
            Algo::Sha3_512 => 72,
            Algo::Shake128 => 168,
            Algo::Blake2s256 => 64,
            _ => 128,
        }
    }
}

macro_rules! fixed_digests {
    ($m:ident) => {
        $m! {
            Md5 => md5::Md5,
            Sha1 => sha1::Sha1,
            Sha224 => sha2::Sha224,
            Sha256 => sha2::Sha256,
            Sha384 => sha2::Sha384,
            Sha512 => sha2::Sha512,
            Sha512_224 => sha2::Sha512_224,
            Sha512_256 => sha2::Sha512_256,
            Sha3_224 => sha3::Sha3_224,
            Sha3_256 => sha3::Sha3_256,
            Sha3_384 => sha3::Sha3_384,
            Sha3_512 => sha3::Sha3_512,
            Ripemd160 => ripemd::Ripemd160,
            Blake2b512 => blake2::Blake2b512,
            Blake2s256 => blake2::Blake2s256,
            Sm3 => sm3::Sm3
        }
    };
}

macro_rules! define_hasher {
    ($($variant:ident => $ty:ty),*) => {
        /// An in-progress digest; `Clone` backs `Hash.copy()`.
        #[derive(Clone)]
        pub enum Hasher {
            $($variant($ty),)*
            Shake128(sha3::Shake128),
            Shake256(sha3::Shake256),
            Md5Sha1(md5::Md5, sha1::Sha1),
        }

        impl Hasher {
            pub fn new(algo: Algo) -> Hasher {
                match algo {
                    $(Algo::$variant => Hasher::$variant(<$ty>::default()),)*
                    Algo::Shake128 => Hasher::Shake128(Default::default()),
                    Algo::Shake256 => Hasher::Shake256(Default::default()),
                    Algo::Md5Sha1 => Hasher::Md5Sha1(Default::default(), Default::default()),
                }
            }

            pub fn update(&mut self, data: &[u8]) {
                match self {
                    $(Hasher::$variant(h) => Digest::update(h, data),)*
                    Hasher::Shake128(h) => Update::update(h, data),
                    Hasher::Shake256(h) => Update::update(h, data),
                    Hasher::Md5Sha1(a, b) => {
                        Digest::update(a, data);
                        Digest::update(b, data);
                    }
                }
            }

            /// The digest, `len` bytes long for the XOFs (their default length otherwise).
            pub fn finish_len(self, len: usize) -> Vec<u8> {
                match self {
                    $(Hasher::$variant(h) => h.finalize().to_vec(),)*
                    Hasher::Shake128(h) => {
                        let mut out = vec![0u8; len];
                        h.finalize_xof_into(&mut out);
                        out
                    }
                    Hasher::Shake256(h) => {
                        let mut out = vec![0u8; len];
                        h.finalize_xof_into(&mut out);
                        out
                    }
                    Hasher::Md5Sha1(a, b) => {
                        let mut out = a.finalize().to_vec();
                        out.extend_from_slice(&b.finalize());
                        out
                    }
                }
            }
        }
    };
}
fixed_digests!(define_hasher);

impl Hasher {
    pub fn finish(self) -> Vec<u8> {
        let len = self.algo().out_len();
        self.finish_len(len)
    }

    pub fn algo(&self) -> Algo {
        match self {
            Hasher::Md5(_) => Algo::Md5,
            Hasher::Sha1(_) => Algo::Sha1,
            Hasher::Sha224(_) => Algo::Sha224,
            Hasher::Sha256(_) => Algo::Sha256,
            Hasher::Sha384(_) => Algo::Sha384,
            Hasher::Sha512(_) => Algo::Sha512,
            Hasher::Sha512_224(_) => Algo::Sha512_224,
            Hasher::Sha512_256(_) => Algo::Sha512_256,
            Hasher::Sha3_224(_) => Algo::Sha3_224,
            Hasher::Sha3_256(_) => Algo::Sha3_256,
            Hasher::Sha3_384(_) => Algo::Sha3_384,
            Hasher::Sha3_512(_) => Algo::Sha3_512,
            Hasher::Ripemd160(_) => Algo::Ripemd160,
            Hasher::Blake2b512(_) => Algo::Blake2b512,
            Hasher::Blake2s256(_) => Algo::Blake2s256,
            Hasher::Sm3(_) => Algo::Sm3,
            Hasher::Shake128(_) => Algo::Shake128,
            Hasher::Shake256(_) => Algo::Shake256,
            Hasher::Md5Sha1(..) => Algo::Md5Sha1,
        }
    }
}

pub fn digest(algo: Algo, data: &[u8]) -> Vec<u8> {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

// ---- HMAC ---------------------------------------------------------------------------------------

macro_rules! define_hmac {
    ($($variant:ident => $ty:ty),*) => {
        /// A keyed, in-progress HMAC.
        #[derive(Clone)]
        pub enum Hmac {
            $($variant(SimpleHmac<$ty>),)*
        }

        impl Hmac {
            /// `None` for the algorithms HMAC is not defined over (XOFs, md5-sha1).
            pub fn new(algo: Algo, key: &[u8]) -> Option<Hmac> {
                Some(match algo {
                    $(Algo::$variant => Hmac::$variant(<SimpleHmac<$ty> as Mac>::new_from_slice(key).ok()?),)*
                    _ => return None,
                })
            }

            pub fn update(&mut self, data: &[u8]) {
                match self {
                    $(Hmac::$variant(m) => Mac::update(m, data),)*
                }
            }

            pub fn finish(self) -> Vec<u8> {
                match self {
                    $(Hmac::$variant(m) => m.finalize().into_bytes().to_vec(),)*
                }
            }
        }
    };
}
fixed_digests!(define_hmac);

pub fn hmac(algo: Algo, key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut h = Hmac::new(algo, key).expect("HMAC is defined over this digest");
    h.update(data);
    h.finish()
}

// ---- KDFs ---------------------------------------------------------------------------------------

macro_rules! define_pbkdf2 {
    ($($variant:ident => $ty:ty),*) => {
        pub fn pbkdf2(algo: Algo, password: &[u8], salt: &[u8], iterations: u32, keylen: usize) -> Vec<u8> {
            let mut out = vec![0u8; keylen];
            match algo {
                $(Algo::$variant => {
                    let _ = pbkdf2::pbkdf2::<SimpleHmac<$ty>>(password, salt, iterations, &mut out);
                })*
                _ => unreachable!("PBKDF2 over a digest HMAC is not defined for"),
            }
            out
        }
    };
}
fixed_digests!(define_pbkdf2);

macro_rules! define_hkdf {
    ($($variant:ident => $ty:ty),*) => {
        pub fn hkdf(algo: Algo, ikm: &[u8], salt: &[u8], info: &[u8], keylen: usize) -> Vec<u8> {
            let mut out = vec![0u8; keylen];
            let salt = if salt.is_empty() { None } else { Some(salt) };
            match algo {
                $(Algo::$variant => {
                    let _ = hkdf::SimpleHkdf::<$ty>::new(salt, ikm).expand(info, &mut out);
                })*
                _ => unreachable!("HKDF over a digest HMAC is not defined for"),
            }
            out
        }
    };
}
fixed_digests!(define_hkdf);

/// Whether HMAC/PBKDF2/HKDF are defined over `algo`.
pub fn supports_mac(algo: Algo) -> bool {
    !matches!(algo, Algo::Shake128 | Algo::Shake256 | Algo::Md5Sha1)
}

pub fn scrypt(password: &[u8], salt: &[u8], n: u64, r: u32, p: u32, keylen: usize) -> Result<Vec<u8>, String> {
    if n < 2 || !n.is_power_of_two() {
        return Err("Invalid scrypt params".into());
    }
    let params = scrypt::Params::new(n.trailing_zeros() as u8, r, p, keylen.max(10)).map_err(|e| e.to_string())?;
    let mut out = vec![0u8; keylen];
    scrypt::scrypt(password, salt, &params, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(hex(&digest(Algo::Sha256, b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&hmac(Algo::Sha1, b"key", b"The quick brown fox jumps over the lazy dog")), "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9");
    }
}
