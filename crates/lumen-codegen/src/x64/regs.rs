//! x86-64 registers and the host calling convention (Win64 or System V).

use crate::machinst::{PReg, RegSet};
use crate::regalloc::RegInfo;

pub const RAX: PReg = PReg::int(0);
pub const RCX: PReg = PReg::int(1);
pub const RDX: PReg = PReg::int(2);
pub const RBX: PReg = PReg::int(3);
pub const RSP: PReg = PReg::int(4);
pub const RBP: PReg = PReg::int(5);
pub const RSI: PReg = PReg::int(6);
pub const RDI: PReg = PReg::int(7);
pub const R8: PReg = PReg::int(8);
pub const R9: PReg = PReg::int(9);
/// Scratch: operand reloads and memory-to-memory moves. Never allocated.
pub const R10: PReg = PReg::int(10);
/// Scratch: operand reloads and parallel-move cycles. Never allocated.
pub const R11: PReg = PReg::int(11);
pub const R12: PReg = PReg::int(12);
pub const R13: PReg = PReg::int(13);
pub const R14: PReg = PReg::int(14);
pub const R15: PReg = PReg::int(15);

pub const fn xmm(n: u8) -> PReg {
    PReg::float(n)
}

#[derive(Clone, Copy)]
pub struct Abi {
    pub int_args: &'static [PReg],
    pub float_args: &'static [PReg],
    /// Win64: argument slots are positional across classes (the 2nd argument uses RDX or XMM1).
    pub positional: bool,
    /// Bytes of home space the caller reserves below stack arguments (32 on Win64).
    pub shadow: u32,
    pub callee_saved: RegSet,
    /// Caller-saved registers a call overwrites.
    pub call_clobbers: RegSet,
    pub win64: bool,
    /// Float scratch registers (caller-saved, never allocated): operand reloads, parallel-move
    /// cycles and memory-to-memory moves, like R10/R11 for integers.
    pub fscratch: [PReg; 2],
}

const WIN_INT_ARGS: [PReg; 4] = [RCX, RDX, R8, R9];
const WIN_FLOAT_ARGS: [PReg; 4] = [xmm(0), xmm(1), xmm(2), xmm(3)];
const SYSV_INT_ARGS: [PReg; 6] = [RDI, RSI, RDX, RCX, R8, R9];
const SYSV_FLOAT_ARGS: [PReg; 8] = [xmm(0), xmm(1), xmm(2), xmm(3), xmm(4), xmm(5), xmm(6), xmm(7)];

pub fn host_abi() -> Abi {
    if cfg!(windows) {
        win64()
    } else {
        sysv()
    }
}

pub fn win64() -> Abi {
    let callee_saved = RegSet::of(&[
        RBX,
        RSI,
        RDI,
        R12,
        R13,
        R14,
        R15,
        xmm(6),
        xmm(7),
        xmm(8),
        xmm(9),
        xmm(10),
        xmm(11),
        xmm(12),
        xmm(13),
        xmm(14),
        xmm(15),
    ]);
    Abi {
        int_args: &WIN_INT_ARGS,
        float_args: &WIN_FLOAT_ARGS,
        positional: true,
        shadow: 32,
        callee_saved,
        call_clobbers: clobbers_of(callee_saved),
        win64: true,
        fscratch: [xmm(4), xmm(5)],
    }
}

pub fn sysv() -> Abi {
    let callee_saved = RegSet::of(&[RBX, R12, R13, R14, R15]);
    Abi {
        int_args: &SYSV_INT_ARGS,
        float_args: &SYSV_FLOAT_ARGS,
        positional: false,
        shadow: 0,
        callee_saved,
        call_clobbers: clobbers_of(callee_saved),
        win64: false,
        fscratch: [xmm(14), xmm(15)],
    }
}

fn clobbers_of(callee_saved: RegSet) -> RegSet {
    let mut s = RegSet::EMPTY;
    for hw in 0..16 {
        let r = PReg::int(hw);
        if r != RSP && r != RBP && !callee_saved.contains(r) {
            s.insert(r);
        }
        let f = xmm(hw);
        if !callee_saved.contains(f) {
            s.insert(f);
        }
    }
    s
}

impl Abi {
    /// Allocation order: caller-saved first, then callee-saved (RyuJIT's `REG_VAR_ORDER`).
    pub fn reg_info(&self) -> RegInfo {
        let ints_caller = [RAX, RCX, RDX, R8, R9, RSI, RDI];
        let ints_callee = [RBX, RSI, RDI, R14, R15, R13, R12];
        let mut int = Vec::new();
        for r in ints_caller {
            if !self.callee_saved.contains(r) && !int.contains(&r) {
                int.push(r);
            }
        }
        for r in ints_callee {
            if self.callee_saved.contains(r) && !int.contains(&r) {
                int.push(r);
            }
        }
        let mut float = Vec::new();
        for callee in [false, true] {
            for n in 0..16 {
                let r = xmm(n);
                if self.callee_saved.contains(r) == callee && !self.fscratch.contains(&r) {
                    float.push(r);
                }
            }
        }
        RegInfo {
            order: [int, float],
            callee_saved: self.callee_saved,
        }
    }
}
