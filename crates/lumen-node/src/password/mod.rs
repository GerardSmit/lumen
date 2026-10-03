//! `Bun.password` backing on the RustCrypto `argon2` and `bcrypt` crates (SHA-512 from `lumen_common::hash`
//! for the bcrypt long-password pre-hash). Lumen carries no cryptography of its own; this
//! module only shapes inputs, strings and errors. Behavior is matched against Bun v1.2.21,
//! see `tests/fixtures/bun_hash_oracle.txt`.
//!
//! Bun behaviors replicated deliberately:
//! - argon2 defaults: argon2id, v=19, m=65536 KiB, t=2, p=1, 32-byte salt, 32-byte tag,
//!   standard base64 without padding in the PHC string.
//! - bcrypt emits `$2b$`, cost 4..=31 (default 10), and pre-hashes passwords longer
//!   than 72 bytes with SHA-512 (the raw 64-byte digest becomes the key). The C-string
//!   NUL is appended to the (truncated) key, exactly like OpenBSD.
//! - verify auto-detects the algorithm from the prefix; `$2a$`/`$2x$`/`$2y$` are all
//!   accepted as bcrypt aliases (Bun verifies every minor identically). Errors carry
//!   Bun's exact "Password verification failed with error \"...\"" messages.
//!
//! Not preserved: the argon2 crate rejects `m < 8 * p`, where Zig (and so Bun) clamped the
//! matrix and still recorded the requested `m`; and associated data is limited to 32 bytes.

use argon2::password_hash::{
    Error as PhError, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, AssociatedData, ParamsBuilder, Version};
use base64::Engine;
use crate::hash::{digest, Algo};
use subtle::ConstantTimeEq;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Argon2Variant {
    Argon2d,
    Argon2i,
    Argon2id,
}

impl Argon2Variant {
    pub fn from_phc(s: &str) -> Option<Argon2Variant> {
        match s {
            "argon2d" => Some(Argon2Variant::Argon2d),
            "argon2i" => Some(Argon2Variant::Argon2i),
            "argon2id" => Some(Argon2Variant::Argon2id),
            _ => None,
        }
    }

    fn algorithm(self) -> Algorithm {
        match self {
            Argon2Variant::Argon2d => Algorithm::Argon2d,
            Argon2Variant::Argon2i => Algorithm::Argon2i,
            Argon2Variant::Argon2id => Algorithm::Argon2id,
        }
    }
}

pub struct Argon2Params {
    pub m_cost: u32,
    pub t_cost: u32,
    pub lanes: u32,
    pub variant: Argon2Variant,
    pub out_len: usize,
    pub secret: Vec<u8>,
    pub associated_data: Vec<u8>,
}

fn argon2_context<'k>(params: &Argon2Params, secret: &'k [u8]) -> Result<Argon2<'k>, String> {
    let ad = AssociatedData::new(&params.associated_data).map_err(|e| e.to_string())?;
    let built = ParamsBuilder::new()
        .m_cost(params.m_cost)
        .t_cost(params.t_cost)
        .p_cost(params.lanes)
        .output_len(params.out_len)
        .data(ad)
        .build()
        .map_err(|e| e.to_string())?;
    Argon2::new_with_secret(secret, params.variant.algorithm(), Version::V0x13, built)
        .map_err(|e| e.to_string())
}

pub fn argon2_hash(password: &[u8], salt: &[u8], params: &Argon2Params) -> Result<Vec<u8>, String> {
    let ctx = argon2_context(params, &params.secret)?;
    let mut out = vec![0u8; params.out_len];
    ctx.hash_password_into(password, salt, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// `$argon2X$v=19$m=..,t=..,p=1$salt$tag` exactly as Bun emits it (p=1, 32-byte tag).
pub fn argon2_phc(
    password: &[u8],
    salt: &[u8],
    m: u32,
    t: u32,
    variant: Argon2Variant,
) -> Result<String, String> {
    let params = Argon2Params {
        m_cost: m,
        t_cost: t,
        lanes: 1,
        variant,
        out_len: 32,
        secret: Vec::new(),
        associated_data: Vec::new(),
    };
    let ctx = argon2_context(&params, &[])?;
    let salt = SaltString::encode_b64(salt).map_err(|e| e.to_string())?;
    ctx.hash_password(password, &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn bcrypt_key(password: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if password.len() > 72 {
        std::borrow::Cow::Owned(digest(Algo::Sha512, password))
    } else {
        std::borrow::Cow::Borrowed(password)
    }
}

/// `$2b$NN$<22 salt chars><31 digest chars>`.
pub fn bcrypt_string(password: &[u8], salt: &[u8; 16], cost: u32) -> String {
    lumen_common::crypt::bcrypt_string('b', &bcrypt_key(password), salt, cost)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasswordError {
    UnsupportedAlgorithm,
    InvalidEncoding,
}

impl PasswordError {
    /// Bun's exact error message text (its `code` is derived from this in the JS glue).
    pub fn message(&self) -> &'static str {
        match self {
            PasswordError::UnsupportedAlgorithm => {
                "Password verification failed with error \"UnsupportedAlgorithm\""
            }
            PasswordError::InvalidEncoding => {
                "Password verification failed with error \"InvalidEncoding\""
            }
        }
    }
}

fn argon2_verify(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(hash).map_err(|_| PasswordError::InvalidEncoding)?;
    match Argon2::default().verify_password(password, &parsed) {
        Ok(()) => Ok(true),
        Err(PhError::Password) => Ok(false),
        Err(_) => Err(PasswordError::InvalidEncoding),
    }
}

fn bcrypt_verify(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    let inv = PasswordError::InvalidEncoding;
    let b = hash.as_bytes();
    if b.len() != 60 || b[6] != b'$' || !hash.is_ascii() {
        return Err(inv);
    }
    let cost: u32 = hash[4..6].parse().map_err(|_| inv)?;
    if !(4..=31).contains(&cost) {
        return Err(inv);
    }
    let salt: [u8; 16] = lumen_common::crypt::bcrypt_decode(&hash[7..29])
        .ok_or(inv)?
        .try_into()
        .map_err(|_| inv)?;
    let expect = lumen_common::crypt::bcrypt_decode(&hash[29..60]).ok_or(inv)?;
    let got = lumen_common::crypt::bcrypt_raw(&bcrypt_key(password), &salt, cost);
    Ok(bool::from(got[..].ct_eq(&expect)))
}

/// Verify `password` against a PHC argon2 string or a `$2[abxy]$` bcrypt string,
/// auto-detecting the algorithm exactly like Bun.
pub fn verify_password(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    if hash.starts_with("$argon2") {
        return argon2_verify(password, hash);
    }
    let b = hash.as_bytes();
    if b.len() > 3
        && b[0] == b'$'
        && b[1] == b'2'
        && matches!(b[2], b'a' | b'b' | b'x' | b'y')
        && b[3] == b'$'
    {
        return bcrypt_verify(password, hash);
    }
    Err(PasswordError::UnsupportedAlgorithm)
}

fn fill_random(buf: &mut [u8]) -> Result<(), String> {
    lumen_os::proc::entropy(buf).map_err(|e| format!("randomness source: {e}"))
}

/// Hash with a fresh random salt. `algorithm` is one of bcrypt/argon2id/argon2i/argon2d;
/// the JS glue has already validated names and ranges with Bun's exact error messages —
/// the checks here are backstops.
pub fn hash_password(
    password: &[u8],
    algorithm: &str,
    m_cost: u32,
    t_cost: u32,
    cost: u32,
) -> Result<String, String> {
    match algorithm {
        "bcrypt" => {
            if !(4..=31).contains(&cost) {
                return Err("Rounds must be between 4 and 31".into());
            }
            let mut salt = [0u8; 16];
            fill_random(&mut salt)?;
            Ok(bcrypt_string(password, &salt, cost))
        }
        "argon2id" | "argon2i" | "argon2d" => {
            if t_cost == 0 {
                return Err("Time cost must be greater than 0".into());
            }
            if m_cost == 0 {
                return Err("Memory cost must be greater than 0".into());
            }
            let variant = Argon2Variant::from_phc(algorithm).expect("matched above");
            let mut salt = [0u8; 32];
            fill_random(&mut salt)?;
            argon2_phc(password, &salt, m_cost, t_cost, variant)
        }
        other => Err(format!("unknown password hashing algorithm: {other}")),
    }
}

// ---- ops (called only by the bun.js glue) ------------------------------------------------------

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__password")]
pub(crate) mod bindings {
    use super::*;
    use lumen::embed::SendError;
    use lumen_bind::{NativeError, NativeResult};

    /// Saturating f64 to u32 (the glue has already validated sign/type per Bun's messages).
    fn sat_u32(n: f64) -> u32 {
        if n.is_nan() {
            0
        } else {
            n.clamp(0.0, u32::MAX as f64) as u32
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn argon2_params(
        who: &str,
        algorithm: &str,
        lanes: f64,
        out_len: f64,
        m_cost: f64,
        t_cost: f64,
        secret: Vec<u8>,
        associated_data: Vec<u8>,
    ) -> NativeResult<Argon2Params> {
        let variant = Argon2Variant::from_phc(algorithm)
            .ok_or_else(|| NativeError::type_error(format!("{who}: invalid Argon2 algorithm")))?;
        Ok(Argon2Params {
            lanes: sat_u32(lanes),
            out_len: sat_u32(out_len) as usize,
            m_cost: sat_u32(m_cost),
            t_cost: sat_u32(t_cost),
            variant,
            secret,
            associated_data,
        })
    }

    #[op(name = "hashSync")]
    pub fn hash_sync(password: &[u8], algorithm: &str, m: f64, t: f64, cost: f64) -> NativeResult<String> {
        hash_password(password, algorithm, sat_u32(m), sat_u32(t), sat_u32(cost)).map_err(NativeError::runtime)
    }

    #[op(name = "verifySync")]
    pub fn verify_sync(password: &[u8], hash: &str) -> NativeResult<bool> {
        verify_password(password, hash).map_err(|e| NativeError::runtime(e.message()))
    }

    #[op(async, name = "hash")]
    pub fn hash_async(password: Vec<u8>, algorithm: String, m: f64, t: f64, cost: f64) -> Result<String, SendError> {
        hash_password(&password, &algorithm, sat_u32(m), sat_u32(t), sat_u32(cost))
            .map_err(|e| SendError::new("Error", e))
    }

    #[op(async, name = "verify")]
    pub fn verify_async(password: Vec<u8>, hash: String) -> Result<bool, SendError> {
        verify_password(&password, &hash).map_err(|e| SendError::new("Error", e.message()))
    }

    #[op(name = "argon2Sync")]
    #[allow(clippy::too_many_arguments)]
    pub fn argon2_sync(
        algorithm: &str,
        message: &[u8],
        nonce: &[u8],
        lanes: f64,
        out_len: f64,
        m_cost: f64,
        t_cost: f64,
        secret: Vec<u8>,
        associated_data: Vec<u8>,
    ) -> NativeResult<Vec<u8>> {
        let params = argon2_params("crypto.argon2Sync", algorithm, lanes, out_len, m_cost, t_cost, secret, associated_data)?;
        argon2_hash(message, nonce, &params).map_err(NativeError::runtime)
    }

    #[op(async, name = "argon2")]
    #[allow(clippy::too_many_arguments)]
    pub fn argon2_async(
        algorithm: String,
        message: Vec<u8>,
        nonce: Vec<u8>,
        lanes: f64,
        out_len: f64,
        m_cost: f64,
        t_cost: f64,
        secret: Vec<u8>,
        associated_data: Vec<u8>,
    ) -> Result<Vec<u8>, SendError> {
        let params = argon2_params("crypto.argon2", &algorithm, lanes, out_len, m_cost, t_cost, secret, associated_data)
            .map_err(SendError::from)?;
        argon2_hash(&message, &nonce, &params).map_err(|e| SendError::new("Error", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_hashes_verify() {
        let salt32 = [7u8; 32];
        for variant in [
            Argon2Variant::Argon2d,
            Argon2Variant::Argon2i,
            Argon2Variant::Argon2id,
        ] {
            let phc = argon2_phc(b"s3cret", &salt32, 32, 2, variant).unwrap();
            assert_eq!(verify_password(b"s3cret", &phc), Ok(true), "{phc}");
            assert_eq!(verify_password(b"wrong", &phc), Ok(false), "{phc}");
        }
        let salt16 = [9u8; 16];
        let bh = bcrypt_string(b"s3cret", &salt16, 4);
        assert!(bh.starts_with("$2b$04$") && bh.len() == 60, "{bh}");
        assert_eq!(verify_password(b"s3cret", &bh), Ok(true));
        assert_eq!(verify_password(b"wrong", &bh), Ok(false));
        for minor in ["a", "x", "y"] {
            let alias = format!("$2{minor}{}", &bh[3..]);
            assert_eq!(verify_password(b"s3cret", &alias), Ok(true), "{alias}");
        }
    }

    #[test]
    fn argon2_phc_format() {
        let phc = argon2_phc(b"pw", &[1u8; 32], 64, 2, Argon2Variant::Argon2id).unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=64,t=2,p=1$"), "{phc}");
        assert_eq!(phc.split('$').count(), 6);
    }

    #[test]
    fn bcrypt_prehash_boundary() {
        let bcrypt_raw = |pw: &[u8], salt: &[u8; 16], cost: u32| lumen_common::crypt::bcrypt_raw(&bcrypt_key(pw), salt, cost);
        let salt = [3u8; 16];
        let long = vec![b'A'; 100];
        assert_eq!(
            bcrypt_raw(&long, &salt, 4),
            bcrypt_raw(&digest(Algo::Sha512, &long), &salt, 4)
        );
        let pw72 = vec![b'A'; 72];
        assert_ne!(
            bcrypt_raw(&pw72, &salt, 4),
            bcrypt_raw(&digest(Algo::Sha512, &pw72), &salt, 4)
        );
    }

    #[test]
    fn bun_1_2_21_oracle_hashes_verify() {
        let fixtures: Vec<(Vec<u8>, &str)> = vec![
            (
                b"hunter2".to_vec(),
                "$argon2id$v=19$m=64,t=1,p=1$Qlq4y6N6W71yfwUALUE1saUFYrEf6EgHwYFtX0swMtU$3Zy/fZUAYcYmCSCCix/WJ0szZpIJI6SYjmGVcdfGtPw",
            ),
            (
                b"hunter2".to_vec(),
                "$argon2i$v=19$m=64,t=2,p=1$11HY6+bCFDRtMsONMVaCUvq4sISHSuc8ldW0Umyh3Mc$Mw+4ZZ6qKn8yvUNEUdE3FL5Zv2k37VsNNbgP7NAjafA",
            ),
            (
                b"hunter2".to_vec(),
                "$argon2d$v=19$m=64,t=2,p=1$g2sFB+MOMkV4ZtklRVjE+oaIPRRdaak2j8tMtt6Arss$ssJ/R8OCdo3od8rL8by1eheI0t7Yn2fYunxA3PDRlhY",
            ),
            (
                b"hunter2".to_vec(),
                "$2b$04$vVj0LFIN/tWPPW6l9KVfb./2Cr91.SdAmARN0zGOD41OBzylfgoGK",
            ),
            (
                vec![b'A'; 73],
                "$2b$04$VtsMUcJFE9ep38Tn9ECXQuyLfmvdzbZhhQU6r.bm.ByAvuWAAVSC6",
            ),
            (
                vec![0, 255, 128, 1, 170, 85],
                "$2b$04$jz6swG09WNYeIYtvADKPH.jmlAMlOvFuuTPm53ndjH0GiNSPbaty.",
            ),
        ];

        for (password, hash) in fixtures {
            assert_eq!(verify_password(&password, hash), Ok(true), "{hash}");
            assert_eq!(verify_password(b"wrong", hash), Ok(false), "{hash}");
        }
    }

    #[test]
    fn argon2id_rfc9106_secret_and_associated_data_vector() {
        let params = Argon2Params {
            m_cost: 32,
            t_cost: 3,
            lanes: 4,
            variant: Argon2Variant::Argon2id,
            out_len: 32,
            secret: vec![3; 8],
            associated_data: vec![4; 12],
        };
        let out = argon2_hash(&vec![1; 32], &vec![2; 16], &params).unwrap();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
        );
    }

    #[test]
    fn verify_error_shapes() {
        assert_eq!(
            verify_password(b"x", "not-a-hash"),
            Err(PasswordError::UnsupportedAlgorithm)
        );
        assert_eq!(
            verify_password(b"x", "$argon2id$v=19$m=64,t=2,p=1$!!bad!!$tag"),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(b"x", "$argon2id$v=19$m=64,t=0,p=1$AAAAAAAA$AAAAAAAA"),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(
                b"x",
                "$2b$99$......................!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
            ),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(b"x", "$2b$04$tooshort"),
            Err(PasswordError::InvalidEncoding)
        );
    }
}
