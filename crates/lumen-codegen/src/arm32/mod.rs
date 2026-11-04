//! The ARM32 backend: A32 (ARM state) code for ARMv7-A (VFPv3-D16) or ARMv6 (VFPv2, e.g. the
//! ARM1176 of the Raspberry Pi 1 and Zero), hard-float AAPCS.
//! legalize → lower ([`lower`]) → allocate ([`crate::regalloc`]) → emit ([`emit`]).
//!
//! 64-bit IR values (`I64`, which the JS tier uses for NaN-boxed words) are register pairs;
//! the lowering expands every 64-bit operation, so the shared register allocator only ever sees
//! 32-bit integer and double-precision float virtual registers. Pointers are `I32`.
//!
//! Scope: everything the JS tier and the shared IR tests use, except vectors, traps and
//! symbol addresses; those make [`compile`] fail so the caller falls back to another tier.
//! There is no trap trampoline and no AOT image support. The code only uses D0..D15 and VFPv2
//! operations; [`Config::v7`] picks `movw`/`movt`, `dmb` and `rbit` or their ARMv6 sequences.
//!
//! External calls load the callee's address into `r12` (`movw`/`movt`, or `mov` and three `orr`
//! on ARMv6) and `blx r12`; each load is a [`Reloc`] (the shared type) that [`load_aligned`] patches. Runtime helpers
//! ([`rt`]) are embedded by absolute address.

pub mod asm;
pub mod emit;
pub mod inst;
pub mod lower;
pub mod regs;
pub mod rt;

#[cfg(test)]
mod tests;

use crate::cfg::Cfg;
use crate::ir::Function;
use crate::legalize::{legalize, split_critical_edges, Legal};
pub use regs::Abi;

pub use crate::x64::{Compiled, Reloc, TrapConfig};

#[derive(Clone, Copy)]
pub struct Config {
    pub abi: Abi,
    /// Must be `None`: this backend has no trap support.
    pub traps: Option<TrapConfig>,
    /// The target implements ARMv7-A (`movw`, `movt`, `dmb`, `rbit`); otherwise the code is
    /// ARMv6 (`ldrex`/`strex`, `uxtb`, `clz`... but none of those, nor the v6K `clrex`).
    pub v7: bool,
    /// The target has `sdiv`/`udiv` in ARM state (Cortex-A7/A15 and every ARMv8 core: the
    /// Raspberry Pi 2 and later on a 32-bit OS). Requires `v7`.
    pub idiv: bool,
    /// The generated code calls helpers with the hard-float (`eabihf`) convention.
    pub hard_float: bool,
}

impl Config {
    /// The configuration for code run by this process.
    pub fn host(traps: Option<TrapConfig>) -> Config {
        Config {
            abi: regs::aapcs_vfp(),
            traps,
            v7: host_v7(),
            idiv: host_v7() && host_idiv(),
            hard_float: cfg!(any(target_abi = "eabihf", not(target_arch = "arm"))),
        }
    }
}

/// Whether the processor is ARMv7 or later: on Linux, the kernel reports VFPv3 (which only
/// ARMv7 cores have) in `AT_HWCAP`; without it the code is ARMv6, which every hard-float ARM
/// Linux process can run. Other targets assume ARMv7, as does a host that is not ARM (such code
/// is only inspected, never run).
pub fn host_v7() -> bool {
    #[cfg(all(target_arch = "arm", any(target_os = "linux", target_os = "android")))]
    {
        const HWCAP_VFPV3: libc::c_ulong = 1 << 13;
        const HWCAP_VFPV3D16: libc::c_ulong = 1 << 14;
        // SAFETY: `getauxval` only reads the process's auxiliary vector.
        let hwcap = unsafe { libc::getauxval(libc::AT_HWCAP) };
        hwcap & (HWCAP_VFPV3 | HWCAP_VFPV3D16) != 0
    }
    #[cfg(not(all(target_arch = "arm", any(target_os = "linux", target_os = "android"))))]
    {
        true
    }
}

/// Whether the processor divides in ARM state: on Linux, `HWCAP_IDIVA` in `AT_HWCAP`. A host
/// that is not ARM says yes (its code is only inspected, never run); other ARM targets no.
pub fn host_idiv() -> bool {
    #[cfg(all(target_arch = "arm", any(target_os = "linux", target_os = "android")))]
    {
        const HWCAP_IDIVA: libc::c_ulong = 1 << 17;
        // SAFETY: `getauxval` only reads the process's auxiliary vector.
        unsafe { libc::getauxval(libc::AT_HWCAP) & HWCAP_IDIVA != 0 }
    }
    #[cfg(not(all(target_arch = "arm", any(target_os = "linux", target_os = "android"))))]
    {
        cfg!(not(target_arch = "arm"))
    }
}

/// Place `compiled` functions in one block of executable memory and patch their relocations:
/// `resolve(id, addrs)` gives the target of external id `id`, where `addrs[i]` is the address
/// `compiled[i]` was placed at. Returns the memory and `addrs`.
pub fn load(
    compiled: &[Compiled],
    resolve: impl Fn(u32, &[u64]) -> Option<u64>,
) -> Result<(crate::jitmem::ExecMemory, Vec<u64>), String> {
    load_aligned(compiled, 16, resolve)
}

pub fn load_aligned(
    compiled: &[Compiled],
    alignment: usize,
    resolve: impl Fn(u32, &[u64]) -> Option<u64>,
) -> Result<(crate::jitmem::ExecMemory, Vec<u64>), String> {
    if !matches!(alignment, 4 | 16 | 64) {
        return Err("arm32: entry alignment must be 4, 16 or 64".into());
    }
    if compiled
        .iter()
        .any(|function| !function.symbol_loads.is_empty())
    {
        return Err("symbol addresses require native AOT linking".into());
    }
    let mut offs = Vec::with_capacity(compiled.len());
    let mut len = 0;
    for c in compiled {
        len = (len + alignment - 1) & !(alignment - 1);
        offs.push(len);
        len += c.code.len();
    }
    let mut result = Ok(Vec::new());
    let mem = crate::jitmem::ExecMemory::with_len(len, |buf, base| {
        if base as usize % alignment != 0 {
            result = Err("arm32: executable allocator did not honor entry alignment".into());
            return;
        }
        buf.fill(0);
        let addrs: Vec<u64> = offs.iter().map(|&o| base + o as u64).collect();
        for (c, &o) in compiled.iter().zip(&offs) {
            buf[o..o + c.code.len()].copy_from_slice(&c.code);
            for r in &c.relocs {
                let Some(a) = resolve(r.func_id, &addrs) else {
                    result = Err(format!("arm32: unresolved fn {}", r.func_id));
                    return;
                };
                if a >> 32 != 0 {
                    result = Err("arm32: address does not fit in 32 bits".into());
                    return;
                }
                asm::patch_mov32(buf, o + r.offset, a as u32);
            }
        }
        result = Ok(addrs);
    })?;
    Ok((mem, result?))
}

/// Compile `func` (which must verify) to machine code.
pub fn compile(func: &Function, cfg: &Config) -> Result<Compiled, String> {
    compile_owned(func.clone(), cfg)
}

/// [`compile`], consuming `f` (no copy; the IR is freed once lowered).
pub fn compile_owned(f: Function, cfg: &Config) -> Result<Compiled, String> {
    compile_owned_aligned(f, cfg, 1)
}

pub fn compile_owned_aligned(
    mut f: Function,
    cfg: &Config,
    _loop_alignment: usize,
) -> Result<Compiled, String> {
    if !cfg.hard_float {
        return Err("arm32: only the hard-float calling convention is supported".into());
    }
    if cfg.traps.is_some() {
        return Err("arm32: traps are unsupported".into());
    }
    legalize(
        &mut f,
        Legal {
            from_u64: true,
            to_uint: true,
            to_int_sat: true,
            srem_min_neg1: true,
            fcopysign: false,
            checked_i32: true,
            js_to_i32: true,
        },
    );
    crate::opt::remove_unreachable(&mut f);
    split_critical_edges(&mut f);
    let graph = Cfg::with_loops(&f);
    let lowered = lower::lower(&f, &graph, &cfg.abi, cfg.v7, cfg.v7 && cfg.idiv)?;
    drop((graph, f));
    let alloc = crate::regalloc::allocate(&lowered.vcode, &cfg.abi.reg_info());
    let frame = emit::Frame::new(&alloc, lowered.outgoing)?;
    let (code, relocs) = emit::Emitter::new(&lowered.vcode, &alloc, &frame, cfg.v7).emit()?;
    Ok(Compiled {
        unwind: Vec::new(),
        windows_unwind: Vec::new(),
        code,
        relocs,
        direct_calls: Vec::new(),
        symbol_loads: Vec::new(),
    })
}
