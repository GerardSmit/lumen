//! Detached Ed25519 signatures over complete format-7 native blobs.

use super::NativeContainer;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub const SIGNATURE_LEN: usize = 64;
pub const PUBLIC_KEY_LEN: usize = 32;

pub fn public_key(seed: &[u8; 32]) -> [u8; PUBLIC_KEY_LEN] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Sign exact container bytes. The private key is a 32-byte Ed25519 seed.
pub fn sign(blob: &[u8], seed: &[u8; 32]) -> Result<[u8; SIGNATURE_LEN], &'static str> {
    NativeContainer::parse(blob)?;
    Ok(SigningKey::from_bytes(seed).sign(blob).to_bytes())
}

/// Accept only a strict signature from one of the build-time allow-listed keys.
pub fn verify(
    blob: &[u8],
    signature: &[u8; SIGNATURE_LEN],
    allowed_keys: &[[u8; PUBLIC_KEY_LEN]],
) -> Result<(), &'static str> {
    if allowed_keys.is_empty() {
        return Err("no native signing keys are configured");
    }
    let signature = Signature::from_bytes(signature);
    if !allowed_keys.iter().any(|bytes| {
        VerifyingKey::from_bytes(bytes).is_ok_and(|key| key.verify_strict(blob, &signature).is_ok())
    }) {
        return Err("native signature is not trusted");
    }
    NativeContainer::parse(blob)?;
    Ok(())
}
