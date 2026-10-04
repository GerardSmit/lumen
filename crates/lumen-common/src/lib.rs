//! Shared, engine-independent building blocks (big integers, Unicode tables, hashing, byte codecs,
//! calendar and time-zone data) used by the JavaScript engine (`lumen`) and `lumen-py`. Std and
//! `memchr` (vectorized search) only; the `hash` and `compress` features add the RustCrypto / zlib
//! / Brotli / Zstandard crates, and are off by default so the engine carries no others.

pub mod aot;
pub mod audio;
pub mod affine;
pub mod bidi;
pub mod bigint;
pub mod buffer;
pub mod bytes;
pub mod civil;
pub mod codec;
#[cfg(feature = "compress")]
pub mod compress;
#[cfg(feature = "cookies")]
pub mod cookies;
pub mod cors;
pub mod csv;
pub mod executable;
#[cfg(not(target_arch = "wasm32"))]
pub mod fastalloc;
pub mod fasthash;
pub mod float;
pub mod float16;
#[cfg(feature = "compress")]
pub mod font;
#[cfg(feature = "hash")]
pub mod hash;
pub mod http_body;
pub mod json;
pub mod limits;
mod linebreak;
pub mod local_tz;
pub mod lzh;
pub mod memcat;
pub mod mime;
pub mod mt19937;
pub mod native;
pub mod regex;
pub mod scan;
pub mod search;
pub mod siphash;
pub mod smuggle;
pub mod stack;
pub mod strftime;
pub mod target;
pub mod tz;
#[rustfmt::skip]
pub mod tzdata;
pub mod tzrules;
pub mod ucd;
pub mod webgl_glsl;
pub mod worker;
#[rustfmt::skip]
pub mod unicode_db;
pub mod unicode_norm;
pub mod unicode_norm_impl;
pub mod unicode_props;
pub mod url;
pub mod utf;
#[cfg(feature = "video")]
pub mod video;
