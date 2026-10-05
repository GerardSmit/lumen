//! Detached Ed25519 signatures over complete format-7 native blobs, and the signed INSTALL
//! frame policy built on them.

use crate::ed25519::{Signature, Signer, SigningKey, VerifyingKey};
use lumen_common::aot::install::{self, InstallPayload};
use lumen_common::aot::NativeContainer;
use lumen_common::target::TargetSpec;

pub const SIGNATURE_LEN: usize = install::SIGNATURE_LEN;
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

/// Validate one complete INSTALL frame for the running native target before
/// storing or mapping its blob. Release callers pass `allow_unsigned = false`
/// and their configured Ed25519 public-key allow-list.
pub fn authorize_install<'a>(
    bytes: &'a [u8],
    max_payload: usize,
    target: &TargetSpec,
    allowed_keys: &[[u8; PUBLIC_KEY_LEN]],
    allow_unsigned: bool,
) -> Result<InstallPayload<'a>, &'static str> {
    install::authorize_install(
        bytes,
        max_payload,
        target,
        allow_unsigned,
        |blob, signature| verify(blob, signature, allowed_keys),
    )
}

/// Authenticate a stored INSTALL frame without requiring the old target to
/// match the current firmware. Used to report apps requiring a host rebuild.
pub fn authenticate_install<'a>(
    bytes: &'a [u8],
    max_payload: usize,
    allowed_keys: &[[u8; PUBLIC_KEY_LEN]],
    allow_unsigned: bool,
) -> Result<InstallPayload<'a>, &'static str> {
    install::authenticate_install(bytes, max_payload, allow_unsigned, |blob, signature| {
        verify(blob, signature, allowed_keys)
    })
}
