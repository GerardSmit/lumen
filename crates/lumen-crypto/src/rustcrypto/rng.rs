//! The system CSPRNG (`lumen_os::proc::entropy`) as a generator for the RustCrypto crates, which
//! come in two `rand_core` generations: 0.6 (`rsa`, the curve crates) and 0.10 (`crypto-bigint`,
//! `crypto-primes`).

use std::convert::Infallible;

pub struct SysRng;

fn fill(dst: &mut [u8]) {
    lumen_os::proc::entropy(dst).expect("operating system randomness source failed");
}

impl rand_core::RngCore for SysRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        fill(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        fill(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        fill(dst);
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), rand_core::Error> {
        fill(dst);
        Ok(())
    }
}

impl rand_core::CryptoRng for SysRng {}

impl rand_core10::TryRng for SysRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        let mut b = [0u8; 4];
        fill(&mut b);
        Ok(u32::from_le_bytes(b))
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        let mut b = [0u8; 8];
        fill(&mut b);
        Ok(u64::from_le_bytes(b))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        fill(dst);
        Ok(())
    }
}

impl rand_core10::TryCryptoRng for SysRng {}
