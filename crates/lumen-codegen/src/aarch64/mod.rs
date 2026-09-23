//! The AArch64 (ARM64) backend: legalize → lower ([`lower`]) → allocate
//! ([`crate::regalloc`]) → emit ([`emit`]), plus the entry trampoline that runs generated code
//! and catches traps. Targets Apple Silicon macOS, Linux and Android on arm64 (and Windows on
//! ARM64, minus unwind data); see [`regs`] for the ABI variants.
//!
//! The public API and the trap contract mirror [`crate::x64`], and the configuration and output
//! types are shared with it: the trampoline saves the address of a *resume slot* at
//! `[ctx + TrapConfig::entry_sp_offset]` (where `ctx` is parameter 0 of the generated
//! functions); a trap stub — or the guard-page fault handler ([`crate::guard`]) — resumes at the
//! address stored in that slot with `sp` = slot + 8 and `x0` = `code + 1`. Callee-saved
//! registers clobbered by the abandoned frames are restored by the trampoline's own epilogue.
//!
//! External calls load the callee's absolute address from a literal-pool entry
//! (`ldr x16, lit; blr x16`); each entry is a [`Reloc`] exactly as on x64.

pub mod asm;
pub mod emit;
pub mod inst;
pub mod lower;
pub mod regs;

use crate::cfg::Cfg;
use crate::ir::{Function, Signature, Type};
use crate::legalize::{legalize, split_critical_edges, Legal};
use asm::{Asm, LdSt, PairMode, FP, LR, SP};
use lower::ArgLoc;
pub use regs::Abi;

pub use crate::x64::{Compiled, Reloc, TrapConfig};

#[derive(Clone, Copy)]
pub struct Config {
    pub abi: Abi,
    pub traps: Option<TrapConfig>,
}

impl Config {
    pub fn host(traps: Option<TrapConfig>) -> Config {
        Config {
            abi: regs::host_abi(),
            traps,
        }
    }
}

/// Place `compiled` functions (16-byte aligned) in one block of executable memory and patch
/// their relocations: `resolve(id, addrs)` gives the target of external id `id`, where
/// `addrs[i]` is the address `compiled[i]` was placed at. Returns the memory and `addrs`.
pub fn load(
    compiled: &[Compiled],
    resolve: impl Fn(u32, &[u64]) -> Option<u64>,
) -> Result<(crate::jitmem::ExecMemory, Vec<u64>), String> {
    let mut offs = Vec::with_capacity(compiled.len());
    let mut len = 0;
    for c in compiled {
        len = (len + 15) & !15;
        offs.push(len);
        len += c.code.len();
    }
    let mut result = Ok(Vec::new());
    let mem = crate::jitmem::ExecMemory::with_len(len, |buf, base| {
        // Padding decodes as `udf #0`.
        buf.fill(0);
        let addrs: Vec<u64> = offs.iter().map(|&o| base + o as u64).collect();
        for (c, &o) in compiled.iter().zip(&offs) {
            buf[o..o + c.code.len()].copy_from_slice(&c.code);
            for r in &c.relocs {
                let Some(a) = resolve(r.func_id, &addrs) else {
                    result = Err(format!("aarch64: unresolved fn {}", r.func_id));
                    return;
                };
                buf[o + r.offset..o + r.offset + 8].copy_from_slice(&a.to_le_bytes());
            }
        }
        result = Ok(addrs);
    })?;
    Ok((mem, result?))
}

/// Compile `func` (which must verify) to position-independent code.
pub fn compile(func: &Function, cfg: &Config) -> Result<Compiled, String> {
    let mut f = func.clone();
    legalize(
        &mut f,
        Legal {
            from_u64: true,
            to_uint: true,
            to_int_sat: true,
            srem_min_neg1: true,
            fcopysign: false,
        },
    );
    crate::opt::remove_unreachable(&mut f);
    split_critical_edges(&mut f);
    let graph = Cfg::new(&f);
    let lowered = lower::lower(&f, &graph, &cfg.abi)?;
    let alloc = crate::regalloc::allocate(&lowered.vcode, &cfg.abi.reg_info());
    let needs_ctx = lowered.has_traps || cfg.traps.is_some_and(|t| t.stack_limit.is_some());
    if needs_ctx {
        if cfg.traps.is_none() {
            return Err("aarch64: function traps but no TrapConfig was given".into());
        }
        if f.sig.params.first() != Some(&Type::I64) {
            return Err("aarch64: trapping functions take the context pointer first".into());
        }
    }
    let frame = emit::Frame::new(&alloc, lowered.outgoing, needs_ctx)?;
    let (code, relocs) =
        emit::Emitter::new(&lowered.vcode, &alloc, &frame, cfg.traps.as_ref()).emit()?;
    Ok(Compiled { code, relocs })
}

/// Trampoline frame, below `fp`: x19..x28 at -16..-80 (pairs), d8..d15 at -96..-144, then
/// the locals below.
const T_CTX: i64 = -152;
const T_PREV: i64 = -160;
const T_SLOTS: i64 = -168;
const T_RESUME: i64 = -176;
const T_LOCALS: u32 = 176;

/// An entry trampoline for functions of signature `sig`:
/// `extern "C" fn(ctx: *mut u8, func: *const u8, slots: *mut u64) -> u32`.
///
/// It calls `func` with arguments `slots[0..]` (raw bits, as [`crate::eval`] represents them;
/// argument 0 is normally `ctx` itself), stores the result in `slots[0]` and returns 0, or
/// returns `code + 1` when the function traps.
pub fn trampoline(sig: &Signature, cfg: &Config) -> Result<Vec<u8>, String> {
    if sig.results.len() > 1 {
        return Err("aarch64: multiple results are not supported".into());
    }
    let tc = cfg
        .traps
        .ok_or("aarch64: the trampoline needs a TrapConfig")?;
    let off = tc.entry_sp_offset as i64;
    let (locs, out) = lower::arg_locs(&cfg.abi, &sig.params);
    let frame = T_LOCALS + out.div_ceil(16) * 16;
    if frame >= 1 << 24 {
        return Err("aarch64: too many stack arguments".into());
    }
    let (x16, x17) = (asm::X16, asm::X17);
    let mut a = Asm::new();
    let common = a.new_label();
    let resume = a.new_label();

    a.pair(false, false, PairMode::Pre, FP, LR, SP, -16);
    a.mov_sp(FP, SP);
    emit::adjust_sp(&mut a, true, frame);
    for k in 0..5u8 {
        a.pair(
            false,
            false,
            PairMode::Offset,
            19 + 2 * k,
            20 + 2 * k,
            FP,
            -16 * (k as i32 + 1),
        );
    }
    for k in 0..4u8 {
        a.pair(
            false,
            true,
            PairMode::Offset,
            8 + 2 * k,
            9 + 2 * k,
            FP,
            -96 - 16 * k as i32,
        );
    }
    a.ldst(LdSt::StrX, 0, FP, T_CTX);
    a.ldst(LdSt::StrX, 2, FP, T_SLOTS);
    a.ldst_any(LdSt::LdrX, x16, 0, off, x17);
    a.ldst(LdSt::StrX, x16, FP, T_PREV);
    a.adr(x16, resume);
    a.ldst(LdSt::StrX, x16, FP, T_RESUME);
    a.add_imm(true, true, x16, FP, (-T_RESUME) as u32, false);
    a.ldst_any(LdSt::StrX, x16, 0, off, x17);
    // func in x9, slots in x10 (x0..x7 are about to receive arguments).
    a.mov(true, 9, 1);
    a.mov(true, 10, 2);
    for (i, loc) in locs.iter().enumerate() {
        let t = sig.params[i];
        let src = 8 * i as i64;
        let wide = t.bits() == 64;
        match *loc {
            ArgLoc::Reg(r) => {
                let op = match (t.is_float(), wide) {
                    (false, false) => LdSt::LdrW,
                    (false, true) => LdSt::LdrX,
                    (true, false) => LdSt::LdrS,
                    (true, true) => LdSt::LdrD,
                };
                a.ldst_any(op, r.hw, 10, src, 12);
            }
            ArgLoc::Stack(o, bytes) => {
                let (ld, st) = if bytes == 4 {
                    (LdSt::LdrW, LdSt::StrW)
                } else {
                    (LdSt::LdrX, LdSt::StrX)
                };
                a.ldst_any(ld, 11, 10, src, 12);
                a.ldst_any(st, 11, SP, o as i64, 12);
            }
        }
    }
    a.blr(9);
    a.ldst(LdSt::LdrX, 10, FP, T_SLOTS);
    match sig.results.first() {
        Some(Type::F32) => a.fcvt_int(asm::FInt::ToGpr, false, false, 0, 0),
        Some(Type::F64) => a.fcvt_int(asm::FInt::ToGpr, true, true, 0, 0),
        Some(Type::I32) => a.mov(false, 0, 0),
        _ => {}
    }
    if !sig.results.is_empty() {
        a.ldst(LdSt::StrX, 0, 10, 0);
    }
    a.mov_imm(0, 0);

    a.bind(common);
    a.ldst(LdSt::LdrX, 1, FP, T_CTX);
    a.ldst(LdSt::LdrX, 2, FP, T_PREV);
    a.ldst_any(LdSt::StrX, 2, 1, off, x17);
    for k in 0..5u8 {
        a.pair(
            true,
            false,
            PairMode::Offset,
            19 + 2 * k,
            20 + 2 * k,
            FP,
            -16 * (k as i32 + 1),
        );
    }
    for k in 0..4u8 {
        a.pair(
            true,
            true,
            PairMode::Offset,
            8 + 2 * k,
            9 + 2 * k,
            FP,
            -96 - 16 * k as i32,
        );
    }
    a.mov_sp(SP, FP);
    a.pair(true, false, PairMode::Post, FP, LR, SP, 16);
    a.ret();

    // A trap stub (or the fault handler) resumed here with sp just above the resume slot and
    // x0 = code + 1. sp is not 16-aligned yet, so nothing is addressed through it before
    // `common` resets it from fp.
    a.bind(resume);
    a.add_imm(true, false, FP, SP, (-T_RESUME - 8) as u32, false);
    a.b(common);
    a.finish()?;
    Ok(a.buf)
}
