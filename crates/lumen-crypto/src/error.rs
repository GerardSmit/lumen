//! The error every backend operation returns. It carries what Node (and OpenSSL) expose on an
//! error object: the `error:<hex>:<library>::<reason>` message, `code`, `library` and `reason`;
//! the language adapters turn it into their own exception type.

use std::fmt;

/// The kind of exception the adapter raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Error,
    Type,
    Range,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CryptoError {
    pub kind: ErrorKind,
    pub message: String,
    /// `ERR_OSSL_*` for OpenSSL errors, `ERR_CRYPTO_*` for the library's own checks.
    pub code: Option<String>,
    /// OpenSSL's library string (`rsa routines`), when the error came from its error queue.
    pub library: Option<String>,
    /// OpenSSL's reason string (`oaep decoding error`).
    pub reason: Option<String>,
}

pub type Result<T> = std::result::Result<T, CryptoError>;

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CryptoError {}

/// OpenSSL library numbers (`ERR_LIB_*`) whose errors Node prefixes into the `ERR_OSSL_<LIB>_*` code.
const CODE_LIBRARIES: &[(u32, &str)] = &[
    (2, "SYS"),
    (3, "BN"),
    (4, "RSA"),
    (5, "DH"),
    (6, "EVP"),
    (7, "BUF"),
    (8, "OBJ"),
    (9, "PEM"),
    (10, "DSA"),
    (11, "X509"),
    (13, "ASN1"),
    (14, "CONF"),
    (15, "CRYPTO"),
    (16, "EC"),
    (20, "SSL"),
    (32, "BIO"),
    (33, "PKCS7"),
    (34, "X509V3"),
    (35, "PKCS12"),
    (36, "RAND"),
    (37, "DSO"),
    (38, "ENGINE"),
    (39, "OCSP"),
    (40, "UI"),
    (41, "COMP"),
    (42, "ECDSA"),
    (43, "ECDH"),
    (44, "OSSL_STORE"),
    (45, "FIPS"),
    (46, "CMS"),
    (47, "TS"),
    (48, "HMAC"),
    (50, "CT"),
    (51, "ASYNC"),
    (52, "KDF"),
    (53, "SM2"),
];

impl CryptoError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> CryptoError {
        CryptoError { kind, message: message.into(), code: None, library: None, reason: None }
    }

    pub fn error(message: impl Into<String>) -> CryptoError {
        CryptoError::new(ErrorKind::Error, message)
    }

    pub fn range(message: impl Into<String>) -> CryptoError {
        CryptoError::new(ErrorKind::Range, message)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> CryptoError {
        self.code = Some(code.into());
        self
    }

    /// What Node says for an operation the selected backend cannot do and has no fallback for.
    pub fn unsupported() -> CryptoError {
        CryptoError::error("Unsupported crypto operation").with_code("ERR_CRYPTO_UNSUPPORTED_OPERATION")
    }

    /// A failed operation without an OpenSSL error behind it (`ERR_CRYPTO_OPERATION_FAILED`).
    pub fn failed(message: impl Into<String>) -> CryptoError {
        CryptoError::error(message).with_code("ERR_CRYPTO_OPERATION_FAILED")
    }

    /// An error in OpenSSL's own format. `hex` is the eight-digit error code, `library` and
    /// `reason` its strings; the `code` is derived the way Node derives it from the library number.
    pub fn openssl(hex: &str, library: &str, reason: &str) -> CryptoError {
        let number = u32::from_str_radix(hex, 16).unwrap_or(0);
        CryptoError::from_queue_entry(number, library, reason)
    }

    /// The error for a raw OpenSSL error code (`ERR_get_error`) with its library / reason strings.
    pub fn from_queue_entry(number: u32, library: &str, reason: &str) -> CryptoError {
        let lib_number = if number & 0x8000_0000 != 0 { 2 } else { (number >> 23) & 0xff };
        let prefix = CODE_LIBRARIES.iter().find(|(n, _)| *n == lib_number).map(|(_, name)| *name);
        let mut code = String::from("ERR_OSSL_");
        if let Some(prefix) = prefix {
            code.push_str(prefix);
            code.push('_');
        }
        let mut last_underscore = true;
        for c in reason.chars() {
            if c.is_ascii_alphanumeric() {
                code.push(c.to_ascii_uppercase());
                last_underscore = false;
            } else if !last_underscore {
                code.push('_');
                last_underscore = true;
            }
        }
        while code.ends_with('_') {
            code.pop();
        }
        CryptoError {
            kind: ErrorKind::Error,
            message: format!("error:{number:08X}:{library}::{reason}"),
            code: Some(code),
            library: Some(library.to_string()),
            reason: Some(reason.to_string()),
        }
    }

    pub fn is_openssl(&self) -> bool {
        self.library.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_nodes_library_prefixes() {
        let e = CryptoError::openssl("02000079", "rsa routines", "oaep decoding error");
        assert_eq!(e.message, "error:02000079:rsa routines::oaep decoding error");
        assert_eq!(e.code.as_deref(), Some("ERR_OSSL_RSA_OAEP_DECODING_ERROR"));
        assert_eq!(e.library.as_deref(), Some("rsa routines"));
        assert_eq!(e.reason.as_deref(), Some("oaep decoding error"));
        let provider = CryptoError::openssl("1C800064", "Provider routines", "bad decrypt");
        assert_eq!(provider.code.as_deref(), Some("ERR_OSSL_BAD_DECRYPT"));
        let bn = CryptoError::openssl("01800076", "bignum routines", "bits too small");
        assert_eq!(bn.code.as_deref(), Some("ERR_OSSL_BN_BITS_TOO_SMALL"));
        let evp = CryptoError::openssl("03000096", "digital envelope routines", "operation not supported for this keytype");
        assert_eq!(evp.code.as_deref(), Some("ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE"));
    }
}
