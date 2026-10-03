//! `_statistics`: the accelerated pieces of `statistics`.

/// Accelerators for the statistics module.
#[lumen_bind::module(name = "_statistics")]
pub mod _statistics {
    use crate::bind::{NativeError, NativeResult};

    #[op]
    fn _normal_dist_inv_cdf(p: f64, mu: f64, sigma: f64) -> NativeResult<f64> {
        lumen_common::float::normal_inv_cdf(p, mu, sigma).map_err(|_| NativeError::value_error("math domain error"))
    }
}
