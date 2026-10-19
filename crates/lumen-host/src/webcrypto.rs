//! The Web Crypto globals that need no key material: `crypto.getRandomValues`,
//! `crypto.randomUUID` and `crypto.subtle.digest`, shared by every embedder.
//!
//! Install with `lazy_globals::<bindings::Module>`: `crypto`, `Crypto`, `SubtleCrypto` and
//! `CryptoKey` are built when a program first touches one of them.
use lumen::embed::OpError;

pub use crate::random::{fill_random, random_bytes, random_uuid};

#[lumen_bind::module(name = "webCrypto")]
pub mod bindings {
    use super::*;
    use lumen::embed::{Ctx, OpResult, Promise, TaKind, This, Value};

    /// The most `getRandomValues` fills in one call.
    const QUOTA_BYTES: usize = 65536;

    const SUBTLE_SLOT: &str = "#\u{0}webcrypto_subtle";

    #[class(name = "Crypto", hint(js(webidl)))]
    pub struct Crypto;

    #[class(name = "SubtleCrypto", hint(js(webidl)))]
    pub struct SubtleCrypto;

    #[class(name = "CryptoKey", hint(js(webidl)))]
    pub struct CryptoKey;

    /// `globalThis.crypto`.
    #[constant(name = "crypto", enumerable)]
    const CRYPTO: Crypto = Crypto;

    #[methods]
    impl Crypto {
        /// Fills an integer typed array in place and returns it.
        fn get_random_values(&self, ctx: &mut Ctx, array: Value) -> OpResult<Value> {
            let integer = matches!(
                ctx.typed_array_kind(&array),
                Some(
                    TaKind::I8
                        | TaKind::U8
                        | TaKind::U8Clamped
                        | TaKind::I16
                        | TaKind::U16
                        | TaKind::I32
                        | TaKind::U32
                        | TaKind::I64
                        | TaKind::U64
                )
            );
            if !integer {
                return Err(OpError::type_error(
                    "getRandomValues expects an integer typed array",
                ));
            }
            if ctx.typed_array_byte_len(&array).unwrap_or(0) > QUOTA_BYTES {
                return Err(OpError::new(
                    "QuotaExceededError",
                    "getRandomValues: quota (65536 bytes) exceeded",
                ));
            }
            ctx.with_typed_array_bytes_mut(&array, fill_random)
                .transpose()?;
            Ok(array)
        }

        #[method(name = "randomUUID")]
        fn random_uuid(&self) -> OpResult<String> {
            random_uuid()
        }

        /// The same `SubtleCrypto` object on every read.
        #[getter]
        fn subtle(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            if ctx.instance_data::<Crypto>(&this).is_none() {
                return Err(OpError::type_error("Illegal invocation"));
            }
            if let Some(subtle) = ctx.native_private_value_slot(&this, SUBTLE_SLOT) {
                return Ok(subtle);
            }
            let subtle = ctx.new_instance(SubtleCrypto);
            let _ = ctx.define_native_private_value_slot(&this, SUBTLE_SLOT, subtle.clone());
            Ok(subtle)
        }
    }

    fn digest_algorithm(name: &str) -> Option<lumen_common::hash::Algo> {
        use lumen_common::hash::Algo;
        match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Some(Algo::Sha1),
            "SHA-256" => Some(Algo::Sha256),
            "SHA-384" => Some(Algo::Sha384),
            "SHA-512" => Some(Algo::Sha512),
            _ => None,
        }
    }

    fn digest_now(ctx: &mut Ctx, algorithm: &Value, data: &Value) -> OpResult<Value> {
        let name = match algorithm {
            Value::Str(_) => algorithm.clone(),
            Value::Obj(_) => ctx.member_get(algorithm, "name").map_err(OpError::thrown)?,
            _ => Value::Undefined,
        };
        let name = if ctx.to_boolean(&name) {
            ctx.coerce_string(&name).map_err(OpError::thrown)?.to_string()
        } else {
            String::new()
        };
        let algo = digest_algorithm(&name).ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                format!("unsupported digest algorithm '{name}'"),
            )
        })?;
        let digest = ctx
            .with_buffer_source_bytes(data, |bytes| lumen_common::hash::digest(algo, bytes))
            .ok_or_else(|| OpError::type_error("digest expects a BufferSource"))?;
        Ok(ctx.make_array_buffer_from(digest))
    }

    #[methods]
    impl SubtleCrypto {
        /// Resolves to an `ArrayBuffer`; every failure is a rejection.
        fn digest(&self, ctx: &mut Ctx, algorithm: Value, data: Value) -> Promise<Value> {
            Promise::ready(digest_now(ctx, &algorithm, &data))
        }
    }

    #[methods]
    impl CryptoKey {}
}
