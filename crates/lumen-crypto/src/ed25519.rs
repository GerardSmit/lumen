//! Ed25519 (RFC 8032) over `ed25519-dalek`; the single implementation used by Node's crypto,
//! X.509 and the AOT native-image signatures.

pub use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
