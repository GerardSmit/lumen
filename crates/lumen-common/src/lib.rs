//! Shared, engine-independent building blocks (big integers, Unicode tables, hashing) used by
//! the JavaScript engine (`lumen`) and `lumen-py`. Std only, no dependencies.

pub mod bigint;
pub mod fasthash;
pub mod unicode_norm;
pub mod unicode_norm_impl;
pub mod unicode_props;
