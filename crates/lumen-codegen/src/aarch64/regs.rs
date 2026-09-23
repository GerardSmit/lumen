//! AArch64 registers and the host calling convention (AAPCS64, Apple's arm64 variant, or
//! Windows on ARM64).
//!
//! Register use common to every variant:
//! - `x0..x7` / `v0..v7` pass arguments and results; `x8..x15` are further caller-saved
//!   temporaries; `x19..x28` and the low 64 bits of `v8..v15` are callee-saved.
//! - `x16`/`x17` (IP0/IP1) are the emitter's scratch registers and are never allocated.
//! - `x18` is the platform register (reserved on Apple and Windows) and is never touched.
//! - `x29` is the frame pointer and `x30` the link register. Every function saves both in its
//!   frame record, so inside a body `x30` is a third scratch register.
//! - `v30`/`v31` are float scratch registers (operand reloads, parallel-move cycles, popcnt).

use crate::machinst::{PReg, RegSet};
use crate::regalloc::RegInfo;

pub const fn x(n: u8) -> PReg {
    PReg::int(n)
}

pub const fn v(n: u8) -> PReg {
    PReg::float(n)
}

pub const X0: PReg = x(0);
pub const V0: PReg = v(0);

#[derive(Clone, Copy)]
pub struct Abi {
    /// Apple arm64: stack arguments are packed at their natural size and alignment (an I32 or
    /// F32 takes 4 bytes) instead of one 8-byte slot each.
    pub apple: bool,
    /// Windows on ARM64. Same register and stack-argument rules as AAPCS64 for the signatures
    /// the IR can express; kept so the caller can tell (e.g. unwind data, `x18`).
    pub windows: bool,
    pub callee_saved: RegSet,
    /// Caller-saved registers a call overwrites.
    pub call_clobbers: RegSet,
}

const INT_ARGS: [PReg; 8] = [x(0), x(1), x(2), x(3), x(4), x(5), x(6), x(7)];
const FLOAT_ARGS: [PReg; 8] = [v(0), v(1), v(2), v(3), v(4), v(5), v(6), v(7)];

/// Float scratch registers (see the module docs).
pub const FSCRATCH: [PReg; 2] = [v(30), v(31)];

impl Abi {
    pub fn int_args(&self) -> &'static [PReg] {
        &INT_ARGS
    }
    pub fn float_args(&self) -> &'static [PReg] {
        &FLOAT_ARGS
    }
}

pub fn host_abi() -> Abi {
    if cfg!(target_vendor = "apple") {
        apple()
    } else if cfg!(windows) {
        windows()
    } else {
        aapcs64()
    }
}

fn callee_saved() -> RegSet {
    let mut s = RegSet::EMPTY;
    for n in 19..=28 {
        s.insert(x(n));
    }
    for n in 8..=15 {
        s.insert(v(n));
    }
    s
}

fn clobbers() -> RegSet {
    let cs = callee_saved();
    let mut s = RegSet::EMPTY;
    for n in 0..=17 {
        s.insert(x(n));
    }
    s.insert(x(30));
    for n in 0..32 {
        if !cs.contains(v(n)) {
            s.insert(v(n));
        }
    }
    s
}

/// Standard AAPCS64 (Linux, Android).
pub fn aapcs64() -> Abi {
    Abi {
        apple: false,
        windows: false,
        callee_saved: callee_saved(),
        call_clobbers: clobbers(),
    }
}

/// Apple's arm64 ABI (macOS, iOS).
pub fn apple() -> Abi {
    Abi {
        apple: true,
        ..aapcs64()
    }
}

/// Windows on ARM64.
pub fn windows() -> Abi {
    Abi {
        windows: true,
        ..aapcs64()
    }
}

impl Abi {
    /// Allocation order: caller-saved temporaries first, then argument registers (which calls
    /// and the entry claim anyway), then callee-saved.
    pub fn reg_info(&self) -> RegInfo {
        let mut int = Vec::new();
        for n in (9..=15).chain(0..=8).chain(19..=28) {
            int.push(x(n));
        }
        let mut float = Vec::new();
        for n in (16..=29).chain(0..=7).chain(8..=15) {
            float.push(v(n));
        }
        RegInfo {
            order: [int, float],
            callee_saved: self.callee_saved,
        }
    }
}
