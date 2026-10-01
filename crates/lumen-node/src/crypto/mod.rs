//! The native half of `node:crypto` and `crypto.subtle`: stateless byte-in/byte-out operations over
//! the RustCrypto crates. The JS side (src/js/crypto.js, generated from Node's own lib/ sources by
//! tools/cryptogen) implements `internalBinding('crypto')` on top of them, so the classes, error
//! codes and validation are Node's. Keys never live here: a JS `KeyObjectHandle` carries the key as
//! DER (SPKI / PKCS#8) or raw bytes and passes it to the ops that need it.
//!
//! Each submodule exports an `OPS` table; `op_crypto_binding` (called once by preamble.js) builds
//! the `__cryptoBinding` object holding all of them.

use lumen::embed::{Ctx, OpDesc, Value};

mod cipher;
mod core;
mod dh;
mod keys;
mod prime;
mod sign;
mod x509;

const TABLES: &[&[&OpDesc]] = &[
    core::OPS,
    cipher::OPS,
    keys::OPS,
    sign::OPS,
    dh::OPS,
    prime::OPS,
    x509::OPS,
];

pub fn op_crypto_binding(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let ns = Value::Obj(ctx.new_object());
    for table in TABLES {
        for op in *table {
            let f = ctx.op_function(op);
            let _ = ctx.set_member(&ns, op.name, f);
        }
    }
    Ok(ns)
}
