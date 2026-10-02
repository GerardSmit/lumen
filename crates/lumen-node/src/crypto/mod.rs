//! The native half of `node:crypto` and `crypto.subtle`: stateless byte-in/byte-out operations over
//! the RustCrypto crates. The JS side (src/js/crypto.js, generated from Node's own lib/ sources by
//! tools/cryptogen) implements `internalBinding('crypto')` on top of them, so the classes, error
//! codes and validation are Node's. Keys never live here: a JS `KeyObjectHandle` carries the key as
//! DER (SPKI / PKCS#8) or raw bytes and passes it to the ops that need it.
//!
//! Each submodule declares a `#[lumen_bind::module]`; `op_crypto_binding` (called once by
//! preamble.js) builds the `__cryptoBinding` object holding all of them.

use lumen::embed::{Ctx, Value};

mod bignum;
mod cipher;
mod core;
mod dh;
mod keys;
mod prime;
mod rng;
mod sign;
mod x509;

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
