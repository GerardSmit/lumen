//! Prime generation and testing (`generatePrime`, `checkPrime`).

use lumen::embed::{OpError, SendError};

use crate::crypto::send_error;

#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
    use super::*;

    fn range_error(message: &'static str) -> SendError {
        SendError::new("RangeError", message).with_code("ERR_OUT_OF_RANGE")
    }

    fn bit_length(be: &[u8]) -> usize {
        match be.iter().position(|&b| b != 0) {
            Some(i) => (be.len() - i) * 8 - be[i].leading_zeros() as usize,
            None => 0,
        }
    }

    fn compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        let trim = |x: &[u8]| x.iter().position(|&v| v != 0).unwrap_or(x.len());
        let (a, b) = (&a[trim(a)..], &b[trim(b)..]);
        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
    }

    fn generate(
        bits: u32,
        safe: bool,
        add: Option<&[u8]>,
        rem: Option<&[u8]>,
    ) -> Result<Vec<u8>, SendError> {
        if let Some(add) = add {
            if bit_length(add) > bits as usize {
                return Err(range_error("invalid options.add"));
            }
            if rem.is_some_and(|r| compare(r, add) != std::cmp::Ordering::Less) {
                return Err(range_error("invalid options.rem"));
            }
        }
        lumen_crypto::backend()
            .prime_generate(bits, safe, add, rem)
            .map_err(send_error)
    }

    /// A prime of exactly `bits` bits, optionally `safe` and congruent to `rem` modulo `add`.
    #[op(name = "primeGenerate")]
    fn prime_generate(
        bits: u32,
        safe: bool,
        add: Option<Vec<u8>>,
        rem: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, OpError> {
        Ok(generate(bits, safe, add.as_deref(), rem.as_deref())?)
    }

    #[op(async, name = "primeGenerateAsync")]
    fn prime_generate_async(
        bits: u32,
        safe: bool,
        add: Option<Vec<u8>>,
        rem: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, SendError> {
        generate(bits, safe, add.as_deref(), rem.as_deref())
    }

    fn check(candidate: &[u8], checks: u32) -> Result<bool, SendError> {
        lumen_crypto::backend()
            .prime_check(candidate, checks)
            .map_err(send_error)
    }

    #[op(name = "primeCheck")]
    fn prime_check(candidate: &[u8], checks: u32) -> Result<bool, OpError> {
        Ok(check(candidate, checks)?)
    }

    #[op(async, name = "primeCheckAsync")]
    fn prime_check_async(candidate: Vec<u8>, checks: u32) -> Result<bool, SendError> {
        check(&candidate, checks)
    }
}
