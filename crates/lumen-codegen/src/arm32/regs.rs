//! ARM32 registers and the AAPCS-VFP (hard-float) calling convention.
//!
//! - `r0..r3` pass integer arguments and results (a 64-bit value takes an even/odd pair, `r0:r1`
//!   for a result); `d0..d7` pass doubles. Only `d0..d5` are used for arguments, because
//!   `d6`/`d7` are the emitter's float scratch registers.
//! - `r4..r10` and `d8..d15` are callee-saved and allocatable.
//! - `r11`, `r12` and `lr` are the emitter's integer scratch registers and are never allocated.
//!   `lr` is free inside a body because the prologue saved it; the prologue also saves `r11`.
//! - `sp` stays 8-byte aligned after the prologue; the frame is addressed from `sp`.
//!
//! A 64-bit IR value is two independent 32-bit virtual registers (low, high); a double is one
//! virtual register in the float class holding a D register. A single-precision value lives in
//! the low S register of its D register.

use crate::machinst::{PReg, RegSet};
use crate::regalloc::RegInfo;

pub const fn r(n: u8) -> PReg {
    PReg::int(n)
}

pub const fn d(n: u8) -> PReg {
    PReg::float(n)
}

pub const R0: PReg = r(0);
pub const D0: PReg = d(0);

/// Integer scratch registers: `r12` (ip), `lr` and `r11`.
pub const ISCRATCH: [u8; 3] = [12, 14, 11];
/// Float scratch registers.
pub const FSCRATCH: [PReg; 2] = [d(6), d(7)];

const INT_ARGS: [PReg; 4] = [r(0), r(1), r(2), r(3)];
const FLOAT_ARGS: [PReg; 6] = [d(0), d(1), d(2), d(3), d(4), d(5)];

#[derive(Clone, Copy)]
pub struct Abi {
    pub callee_saved: RegSet,
    /// Caller-saved registers a call overwrites.
    pub call_clobbers: RegSet,
}

impl Abi {
    pub fn int_args(&self) -> &'static [PReg] {
        &INT_ARGS
    }
    pub fn float_args(&self) -> &'static [PReg] {
        &FLOAT_ARGS
    }

    /// Allocation order: caller-saved temporaries first, then callee-saved.
    pub fn reg_info(&self) -> RegInfo {
        let mut int = Vec::new();
        for n in (0..=3).chain(4..=10) {
            int.push(r(n));
        }
        let mut float = Vec::new();
        for n in (0..=5).chain(8..=15) {
            float.push(d(n));
        }
        RegInfo {
            order: [int, float],
            callee_saved: self.callee_saved,
        }
    }
}

pub fn aapcs_vfp() -> Abi {
    let mut callee_saved = RegSet::EMPTY;
    for n in 4..=10 {
        callee_saved.insert(r(n));
    }
    for n in 8..=15 {
        callee_saved.insert(d(n));
    }
    let mut call_clobbers = RegSet::EMPTY;
    for n in 0..=3 {
        call_clobbers.insert(r(n));
    }
    for n in ISCRATCH {
        call_clobbers.insert(r(n));
    }
    for n in 0..=7 {
        call_clobbers.insert(d(n));
    }
    Abi {
        callee_saved,
        call_clobbers,
    }
}
