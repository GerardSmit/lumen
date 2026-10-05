//! `CFError` (an `OSStatus` in `NSOSStatusErrorDomain`) to the neutral error, worded the way Node
//! words the OpenSSL error of the same failure.

use core_foundation::error::CFError;

use crate::error::CryptoError;
use crate::rsa_util::{data_too_large, ossl, sign_failed};

const ERR_SEC_UNIMPLEMENTED: i64 = -4;
const ERR_SEC_PARAM: i64 = -50;
const ERR_SEC_ALLOCATE: i64 = -108;
const ERR_SEC_UNSUPPORTED_KEY_SIZE: i64 = -25;
const ERR_SEC_KEY_SIZE_NOT_ALLOWED: i64 = -25293;
const ERR_SEC_DECODE: i64 = -26275;

/// What the failed call was doing.
#[derive(Clone, Copy)]
pub(super) enum Context {
    Encrypt,
    Sign,
    Generate,
}

pub(super) fn map(context: Context, error: &CFError) -> CryptoError {
    status(context, error.code() as i64)
}

fn status(context: Context, code: i64) -> CryptoError {
    match (context, code) {
        (_, ERR_SEC_UNIMPLEMENTED) => CryptoError::unsupported(),
        (_, ERR_SEC_ALLOCATE) => CryptoError::error("Memory allocation failed").with_code("ERR_MEMORY_ALLOCATION_FAILED"),
        (Context::Encrypt, ERR_SEC_PARAM) => data_too_large(),
        (Context::Generate, ERR_SEC_PARAM | ERR_SEC_UNSUPPORTED_KEY_SIZE | ERR_SEC_KEY_SIZE_NOT_ALLOWED) => {
            ossl("1C80006B", "Provider routines", "key size too small")
        }
        (Context::Generate, _) => CryptoError::error("RSA key generation failed"),
        (Context::Sign, ERR_SEC_DECODE) => data_too_large(),
        (Context::Encrypt | Context::Sign, _) => sign_failed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_nodes_errors() {
        assert_eq!(status(Context::Encrypt, ERR_SEC_PARAM).message, "error:0200006E:rsa routines::data too large for key size");
        assert_eq!(status(Context::Sign, ERR_SEC_UNIMPLEMENTED).code.as_deref(), Some("ERR_CRYPTO_UNSUPPORTED_OPERATION"));
        assert_eq!(status(Context::Sign, 7).code.as_deref(), Some("ERR_CRYPTO_OPERATION_FAILED"));
    }
}
