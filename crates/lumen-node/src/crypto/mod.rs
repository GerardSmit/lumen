//! The native half of `node:crypto` and `crypto.subtle`: stateless byte-in/byte-out operations. The
//! JS side (src/js/crypto.js, generated from Node's own lib/ sources by tools/cryptogen)
//! implements `internalBinding('crypto')` on top of them, so the classes, error codes and
//! validation are Node's. Keys never live here: a JS `KeyObjectHandle` carries the key as DER
//! (SPKI / PKCS#8) or raw bytes and passes it to the ops that need it.
//!
//! Symmetric ciphers, EC and EdDSA run on RustCrypto. RSA, DSA, finite-field DH, primes and their
//! key generation go through `lumen_crypto::backend()` (the system OpenSSL when it loads, RustCrypto
//! otherwise); this crate only adapts its neutral types and errors.
//!
//! Each submodule declares a `#[lumen_bind::module]`; `op_crypto_binding` (called once by
//! preamble.js) builds the `__cryptoBinding` object holding all of them.

use lumen::embed::{Ctx, OpError, SendError, Value};
use lumen_crypto::{CryptoError, ErrorKind};

mod cipher;
mod core;
mod dh;
mod keys;
mod prime;
mod sign;
mod x509;

fn error_class(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Error => "Error",
        ErrorKind::Type => "TypeError",
        ErrorKind::Range => "RangeError",
    }
}

/// A backend error as a thread-safe error (`library` / `reason` are recovered from the message by
/// the JS binding, like for every OpenSSL-style error).
pub(crate) fn send_error(e: CryptoError) -> SendError {
    let error = SendError::new(error_class(e.kind), e.message);
    match e.code {
        Some(code) => error.with_code(code),
        None => error,
    }
}

pub(crate) fn op_error(e: CryptoError) -> OpError {
    send_error(e).into()
}

pub fn op_crypto_binding(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let ns = ctx.module_object::<core::bindings::Module>()?;
    ctx.install_module::<cipher::bindings::Module>(&ns)?;
    ctx.install_module::<keys::bindings::Module>(&ns)?;
    ctx.install_module::<sign::bindings::Module>(&ns)?;
    ctx.install_module::<dh::bindings::Module>(&ns)?;
    ctx.install_module::<prime::bindings::Module>(&ns)?;
    ctx.install_module::<x509::bindings::Module>(&ns)?;
    Ok(ns)
}
