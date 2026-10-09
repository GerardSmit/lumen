//! Shared, engine-independent building blocks (big integers, Unicode tables, hashing, byte codecs,
//! calendar and time-zone data) used by the JavaScript engine (`lumen`) and `lumen-py`. Std and
//! `memchr` (vectorized search) and `libm` (portable color math); the `hash` and `compress` features add the RustCrypto / zlib
//! / Brotli / Zstandard crates, and are off by default so the engine carries no others.

extern crate alloc;

pub mod affine;
pub mod dom_geometry;
pub mod aot;
pub mod audio;
pub mod bidi;
pub mod bigint;
pub mod buffer;
pub mod bytes;
pub mod civil;
pub mod codec;
pub mod color;
pub mod filter;
#[cfg(feature = "web-encoding")]
pub mod encoding;
#[cfg(feature = "deflate")]
pub mod compress;
#[cfg(feature = "cookies")]
pub mod cookies;
pub mod cors;
#[cfg(feature = "csp")]
pub mod csp;
#[cfg(feature = "csp")]
pub mod csp_report;
#[cfg(feature = "csp")]
pub mod reporting;
#[cfg(feature = "csp")]
pub mod permissions_policy;
pub mod crc32;
#[cfg(feature = "crypt")]
pub mod crypt;
pub mod csv;
pub mod cycle;
pub mod deadline;
pub mod decimal;
pub mod dedent;
pub mod editdist;
pub mod entities;
pub mod indexed_db;
pub mod import_maps;
#[cfg(feature = "executable")]
pub mod executable;
#[cfg(not(target_arch = "wasm32"))]
pub mod fastalloc;
pub mod fasthash;
pub mod float;
pub mod float16;
pub mod fmtspec;
#[cfg(feature = "compress")]
pub mod font;
#[cfg(feature = "hash")]
pub mod hash;
#[cfg(feature = "hash")]
pub mod integrity;
pub mod history;
pub mod html_autofill;
pub mod html_numbers;
pub mod http_body;
pub mod json;
pub mod limits;
pub mod raster;
mod linebreak;
pub mod lineno;
pub mod local_tz;
pub mod memcat;
pub mod mime;
pub mod srcset;
pub mod referrer;
pub mod mt19937;
pub mod multipart;
pub mod native;
pub mod pointer;
pub mod pem;
pub mod pickle;
pub mod pypath;
pub mod pytime;
pub mod regex;
pub mod rounding;
pub mod performance;
pub mod scan;
pub mod scroll;
pub mod search;
pub mod siphash;
pub mod smuggle;
pub mod stack;
pub mod toggle_task;
pub mod strftime;
pub mod target;
pub mod text;
pub mod case_transform;
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
pub mod wait;
pub mod x509;
pub mod xml;

#[cfg(feature="vector-path")]
pub mod svg_path;
