//! `_crypt`: the shared password hashes of `lumen_common::crypt` (MD5, SHA-256, SHA-512,
//! bcrypt), then the system `crypt(3)` through `lumen_os::crypt` for the formats Lumen does not
//! implement itself (traditional DES, ...).

/// Hashing of passwords with the Unix `crypt()` interface.
#[lumen_bind::module(name = "_crypt")]
pub mod crypt {
    use crate::object::*;
    use crate::vm::Interp;

    /// Hash a *word* with the given *salt* and return the hashed password.
    ///
    /// *word* will usually be a user's password.  *salt* (either a random 2 or 16
    /// character string, possibly prefixed with $digit$ to indicate the method)
    /// will be used to perturb the encryption algorithm and produce distinct
    /// results for a given *word*.
    #[op]
    fn crypt(it: &mut Interp, word: &str, salt: &str) -> R<String> {
        if word.contains('\0') || salt.contains('\0') {
            return Err(it.value_error("embedded null character"));
        }
        if let Some(hashed) = lumen_common::crypt::crypt(word.as_bytes(), salt) {
            return Ok(hashed);
        }
        match lumen_os::crypt::crypt(word.as_bytes(), salt.as_bytes()) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => Err(it.os_error_errno(e.errno(), None, None)),
        }
    }
}
