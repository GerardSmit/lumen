//! Shared, engine-independent building blocks (big integers, Unicode tables, hashing, byte codecs,
//! calendar and time-zone data) used by the JavaScript engine (`lumen`) and `lumen-py`. Std only;
//! the `hash` and `compress` features add the RustCrypto / zlib / Brotli / Zstandard crates, and
//! are off by default so the engine itself carries no dependencies.

pub mod bigint;
pub mod civil;
pub mod codec;
#[cfg(feature = "compress")]
pub mod compress;
#[cfg(not(target_arch = "wasm32"))]
pub mod fastalloc;
pub mod fasthash;
#[cfg(feature = "hash")]
pub mod hash;
pub mod memcat;
pub mod stack;
pub mod regex;
pub mod smuggle;
pub mod tz;
#[rustfmt::skip]
pub mod tzdata;
pub mod unicode_norm;
pub mod unicode_norm_impl;
pub mod unicode_props;
