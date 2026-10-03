//! Shared, engine-independent building blocks (big integers, Unicode tables, hashing, byte codecs,
//! calendar and time-zone data) used by the JavaScript engine (`lumen`) and `lumen-py`. Std and
//! `memchr` (vectorized search) only; the `hash` and `compress` features add the RustCrypto / zlib
//! / Brotli / Zstandard crates, and are off by default so the engine carries no others.

pub mod bigint;
pub mod buffer;
pub mod civil;
pub mod codec;
pub mod csv;
pub mod decimal;
pub mod dedent;
pub mod editdist;
pub mod cycle;
#[cfg(feature = "compress")]
pub mod compress;
#[cfg(not(target_arch = "wasm32"))]
pub mod fastalloc;
pub mod fasthash;
pub mod float;
pub mod fmtspec;
pub mod float16;
#[cfg(feature = "crypt")]
pub mod crypt;
#[cfg(feature = "hash")]
pub mod hash;
pub mod history;
pub mod json;
pub mod limits;
pub mod lineno;
pub mod local_tz;
pub mod memcat;
pub mod mt19937;
pub mod native;
pub mod pytime;
pub mod pem;
pub mod pickle;
pub mod pypath;
pub mod stack;
pub mod regex;
pub mod rounding;
pub mod search;
pub mod siphash;
pub mod smuggle;
pub mod strftime;
pub mod text;
pub mod tz;
#[rustfmt::skip]
pub mod tzdata;
pub mod tzrules;
pub mod ucd;
#[rustfmt::skip]
pub mod unicode_db;
pub mod unicode_norm;
pub mod unicode_norm_impl;
pub mod unicode_props;
pub mod utf;
pub mod xml;
pub mod x509;
pub mod wait;
