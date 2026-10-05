//! RSASSA-PSS encoding (RFC 8017, 9.1) over a raw RSA primitive supplied by the caller. Backends
//! use it when their own PSS cannot express the request (an MGF1 digest other than the message
//! digest, an arbitrary salt length, automatic salt detection). Only public values pass through
//! the encoding; the private-key operation is the caller's.

use lumen_common::hash;

use crate::error::Result;
use crate::rsa_util::{bit_len, data_too_large, mod_len, sign_failed};
use crate::{pad_be, trim_be, Algo, PssSalt};

fn mgf1_xor(algo: Algo, seed: &[u8], data: &mut [u8]) {
    for (counter, chunk) in data.chunks_mut(algo.out_len()).enumerate() {
        let mut block = seed.to_vec();
        block.extend_from_slice(&(counter as u32).to_be_bytes());
        for (d, m) in chunk.iter_mut().zip(hash::digest(algo, &block)) {
            *d ^= m;
        }
    }
}

fn em_len(n_bits: usize) -> usize {
    (n_bits - 1).div_ceil(8)
}

/// The salt length a signature uses: the largest one that fits for [`PssSalt::MaxOrAuto`].
pub(crate) fn sign_salt_len(n: &[u8], hash: Algo, salt: PssSalt) -> Result<usize> {
    let h_len = hash.out_len();
    let max = em_len(bit_len(n)).saturating_sub(h_len + 2);
    let len = match salt {
        PssSalt::MaxOrAuto => max,
        PssSalt::Digest => h_len,
        PssSalt::Length(s) => s as usize,
    };
    if len > max {
        return Err(data_too_large());
    }
    Ok(len)
}

/// Signs `mhash`: encodes it with a fresh salt of `salt_len` bytes and hands the encoded message
/// to `private_op`, the raw private-key operation (input and output big-endian).
pub(crate) fn sign(
    n: &[u8],
    hash_algo: Algo,
    mgf1: Algo,
    mhash: &[u8],
    salt_len: usize,
    private_op: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let em_bits = bit_len(n) - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = hash_algo.out_len();
    if em_len < h_len + salt_len + 2 {
        return Err(data_too_large());
    }
    let mut salt = vec![0u8; salt_len];
    lumen_os::proc::entropy(&mut salt).map_err(|_| sign_failed())?;
    let h = hash::digest(hash_algo, &[&[0u8; 8], mhash, &salt].concat());
    let mut db = vec![0u8; em_len - h_len - 1];
    let one = db.len() - salt_len - 1;
    db[one] = 1;
    db[one + 1..].copy_from_slice(&salt);
    mgf1_xor(mgf1, &h, &mut db);
    db[0] &= 0xff >> (8 * em_len - em_bits);
    let em = [db, h, vec![0xbc]].concat();
    Ok(pad_be(&private_op(&em)?, mod_len(n)))
}

/// Checks the encoded message `em` that the public-key operation recovered from a signature.
pub(crate) fn verify(em: &[u8], n: &[u8], hash_algo: Algo, mgf1: Algo, mhash: &[u8], salt: PssSalt) -> bool {
    let em_bits = bit_len(n) - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = hash_algo.out_len();
    let em = trim_be(em);
    if em.len() > em_len || em_len < h_len + 2 {
        return false;
    }
    let em = pad_be(em, em_len);
    if em[em_len - 1] != 0xbc {
        return false;
    }
    let (masked, rest) = em.split_at(em_len - h_len - 1);
    let h = &rest[..h_len];
    let top = 0xffu8 >> (8 * em_len - em_bits);
    if masked[0] & !top != 0 {
        return false;
    }
    let mut db = masked.to_vec();
    mgf1_xor(mgf1, h, &mut db);
    db[0] &= top;
    let Some(ps) = db.iter().position(|&b| b != 0) else { return false };
    if db[ps] != 1 {
        return false;
    }
    let salt_bytes = &db[ps + 1..];
    let expected = match salt {
        PssSalt::MaxOrAuto => salt_bytes.len(),
        PssSalt::Digest => h_len,
        PssSalt::Length(s) => s as usize,
    };
    if salt_bytes.len() != expected {
        return false;
    }
    hash::constant_time_eq(&hash::digest(hash_algo, &[&[0u8; 8], mhash, salt_bytes].concat()), h)
}
