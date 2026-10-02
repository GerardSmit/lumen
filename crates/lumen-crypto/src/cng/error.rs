//! `NTSTATUS` to the neutral error, worded the way Node words the OpenSSL error of the same failure.

use crate::error::CryptoError;
use crate::rsa_util::{data_too_large, sign_failed};

const STATUS_NO_MEMORY: u32 = 0xC000_0017;
const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
const STATUS_INVALID_BUFFER_SIZE: u32 = 0xC000_0206;
const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
const NTE_NOT_SUPPORTED: u32 = 0x8009_0029;
const NTE_NO_MEMORY: u32 = 0x8009_000E;

/// What the failed call was doing.
#[derive(Clone, Copy)]
pub(super) enum Context {
    Crypt,
    Sign,
    Generate,
}

pub(super) fn map(context: Context, status: i32) -> CryptoError {
    match (context, status as u32) {
        (_, STATUS_NOT_SUPPORTED | NTE_NOT_SUPPORTED) => CryptoError::unsupported(),
        (_, STATUS_NO_MEMORY | NTE_NO_MEMORY) => CryptoError::error("Memory allocation failed").with_code("ERR_MEMORY_ALLOCATION_FAILED"),
        (Context::Crypt, STATUS_INVALID_BUFFER_SIZE | STATUS_BUFFER_TOO_SMALL | STATUS_INVALID_PARAMETER) => data_too_large(),
        (Context::Generate, _) => CryptoError::error("RSA key generation failed"),
        (Context::Crypt | Context::Sign, _) => sign_failed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_nodes_errors() {
        assert_eq!(map(Context::Crypt, STATUS_INVALID_PARAMETER as i32).message, "error:0200006E:rsa routines::data too large for key size");
        assert_eq!(map(Context::Sign, STATUS_NOT_SUPPORTED as i32).code.as_deref(), Some("ERR_CRYPTO_UNSUPPORTED_OPERATION"));
        assert_eq!(map(Context::Sign, 7).code.as_deref(), Some("ERR_CRYPTO_OPERATION_FAILED"));
    }
}
