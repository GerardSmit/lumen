//! The x86-64 backend: legalize → lower ([`lower`]) → allocate ([`crate::regalloc`]) → emit
//! ([`emit`]), plus the entry trampoline that runs generated code and catches traps.
//!
//! Traps unwind without tables: the trampoline saves its stack pointer at
//! `[ctx + TrapConfig::entry_sp_offset]` (where `ctx` is parameter 0 of the generated functions),
//! and a trap stub reloads it and returns into the trampoline with the trap code. Callee-saved
//! registers clobbered by the abandoned frames are restored by the trampoline's own epilogue.

pub mod asm;
pub mod emit;
pub mod inst;
pub mod lower;
pub mod regs;

use crate::cfg::Cfg;
use crate::ir::{Function, Signature, Type};
use crate::legalize::{legalize, split_critical_edges, Legal};
use asm::{Asm, RM};
use lower::ArgLoc;
pub use regs::Abi;

/// Optional instruction-set extensions (everything else is x86-64 baseline plus SSE2).
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    pub lzcnt: bool,
    pub bmi1: bool,
    pub bmi2: bool,
    pub popcnt: bool,
    pub sse41: bool,
}

impl Features {
    pub fn host() -> Features {
        #[cfg(target_arch = "x86_64")]
        {
            Features {
                lzcnt: std::arch::is_x86_feature_detected!("lzcnt"),
                bmi1: std::arch::is_x86_feature_detected!("bmi1"),
                bmi2: std::arch::is_x86_feature_detected!("bmi2"),
                popcnt: std::arch::is_x86_feature_detected!("popcnt"),
                sse41: std::arch::is_x86_feature_detected!("sse4.1"),
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        Features::default()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TrapConfig {
    /// Where the trampoline keeps its stack pointer, relative to the context pointer.
    pub entry_sp_offset: i32,
    /// `(offset of the stack limit in the context, trap code)`: functions trap when `rsp`
    /// falls below `[ctx + offset]`.
    pub stack_limit: Option<(i32, u32)>,
}

#[derive(Clone, Copy)]
pub struct Config {
    pub abi: Abi,
    pub features: Features,
    pub traps: Option<TrapConfig>,
}

impl Config {
    pub fn host(traps: Option<TrapConfig>) -> Config {
        Config {
            abi: regs::host_abi(),
            features: Features::host(),
            traps,
        }
    }
}

/// A 64-bit absolute address of external function `func_id`, to patch at `offset`.
#[derive(Clone, Copy, Debug)]
pub struct Reloc {
    pub offset: usize,
    pub func_id: u32,
}

pub struct Compiled {
    pub code: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

impl Compiled {
    /// Patch every relocation with the address `resolve` gives its function.
    pub fn link(&mut self, resolve: impl Fn(u32) -> Option<u64>) -> Result<(), String> {
        for r in &self.relocs {
            let addr = resolve(r.func_id).ok_or_else(|| format!("x64: unresolved fn {}", r.func_id))?;
            self.code[r.offset..r.offset + 8].copy_from_slice(&addr.to_le_bytes());
        }
        Ok(())
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
        buf.fill(0xcc);
        let addrs: Vec<u64> = offs.iter().map(|&o| base + o as u64).collect();
        for (c, &o) in compiled.iter().zip(&offs) {
            buf[o..o + c.code.len()].copy_from_slice(&c.code);
            for r in &c.relocs {
                let Some(a) = resolve(r.func_id, &addrs) else {
                    result = Err(format!("x64: unresolved fn {}", r.func_id));
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
            from_u64: false,
            to_uint: false,
            to_int_sat: false,
            srem_min_neg1: false,
            fcopysign: false,
        },
    );
    crate::opt::remove_unreachable(&mut f);
    split_critical_edges(&mut f);
    let graph = Cfg::new(&f);
    let lowered = lower::lower(&f, &graph, &cfg.abi, &cfg.features)?;
    let alloc = crate::regalloc::allocate(&lowered.vcode, &cfg.abi.reg_info());
    let needs_ctx = lowered.has_traps || cfg.traps.is_some_and(|t| t.stack_limit.is_some());
    if needs_ctx {
        if cfg.traps.is_none() {
            return Err("x64: function traps but no TrapConfig was given".into());
        }
        if f.sig.params.first() != Some(&Type::I64) {
            return Err("x64: trapping functions take the context pointer first".into());
        }
    }
    let frame = emit::Frame::new(&alloc, lowered.outgoing, needs_ctx);
    let traps = cfg.traps.as_ref();
    let (_, fits, _) =
        emit::Emitter::new(&lowered.vcode, &alloc, &cfg.abi, &frame, traps, Vec::new()).emit()?;
    let (code, _, relocs) =
        emit::Emitter::new(&lowered.vcode, &alloc, &cfg.abi, &frame, traps, fits).emit()?;
    Ok(Compiled { code, relocs })
}

/// The callee-saved registers of either ABI, which the trampoline saves unconditionally.
const TRAMP_GPRS: [u8; 7] = [3, 6, 7, 12, 13, 14, 15];

/// An entry trampoline for functions of signature `sig`:
/// `extern "C" fn(ctx: *mut u8, func: *const u8, slots: *mut u64) -> u32`.
///
/// It calls `func` with arguments `slots[0..]` (raw bits, as [`crate::eval`] represents them;
/// argument 0 is normally `ctx` itself), stores the result in `slots[0]` and returns 0, or
/// returns `code + 1` when the function traps.
pub fn trampoline(sig: &Signature, cfg: &Config) -> Result<Vec<u8>, String> {
    if sig.results.len() > 1 {
        return Err("x64: multiple results are not supported".into());
    }
    let tc = cfg.traps.ok_or("x64: the trampoline needs a TrapConfig")?;
    let abi = &cfg.abi;
    let off = tc.entry_sp_offset;
    let (ctx_s, prev_s, slots_s, resume_s) = (-64, -72, -80, -88);
    let xmm_base = -256;
    let mut a = Asm::new(Vec::new());
    let common = a.new_label();
    let resume = a.new_label();

    a.push(asm::RBP);
    a.mov_rr(asm::RBP, asm::RSP);
    for &g in &TRAMP_GPRS {
        a.push(g);
    }
    let (locs, out) = lower::arg_locs(abi, &sig.params);
    let locals: u32 = if abi.win64 { 256 - 56 } else { 88 - 56 };
    let mut size = locals + out;
    while (56 + size) % 16 != 0 {
        size += 8;
    }
    a.alu_imm(true, 5, RM::Reg(asm::RSP), size as i32);
    if abi.win64 {
        for k in 0..10u8 {
            let m = RM::mem(asm::RBP, xmm_base + 16 * k as i32);
            a.op(0xf3, false, &[0x0f, 0x7f], 6 + k, m, false, 0);
        }
    }
    let [a0, a1, a2] = [abi.int_args[0].hw, abi.int_args[1].hw, abi.int_args[2].hw];
    a.mov_store(true, a0, RM::mem(asm::RBP, ctx_s));
    a.mov_store(true, a2, RM::mem(asm::RBP, slots_s));
    a.mov_load(true, asm::RAX, RM::mem(a0, off));
    a.mov_store(true, asm::RAX, RM::mem(asm::RBP, prev_s));
    a.op(0, true, &[0x8d], asm::RAX, RM::Rip(resume), false, 0);
    a.mov_store(true, asm::RAX, RM::mem(asm::RBP, resume_s));
    a.op(0, true, &[0x8d], asm::RAX, RM::mem(asm::RBP, resume_s), false, 0);
    a.mov_store(true, asm::RAX, RM::mem(a0, off));
    a.mov_rr(asm::R11, a1);
    a.mov_rr(asm::R10, a2);
    for (i, loc) in locs.iter().enumerate() {
        let src = RM::mem(asm::R10, 8 * i as i32);
        match *loc {
            ArgLoc::Stack(o) => {
                a.mov_load(true, asm::RAX, src);
                a.mov_store(true, asm::RAX, RM::mem(asm::RSP, o));
            }
            ArgLoc::Reg(r) if sig.params[i].is_float() => {
                a.op(0xf2, false, &[0x0f, 0x10], r.hw, src, false, 0)
            }
            ArgLoc::Reg(r) => a.mov_load(true, r.hw, src),
        }
    }
    a.bytes(&[0x41, 0xff, 0xd3]); // call r11
    a.mov_load(true, asm::R10, RM::mem(asm::RBP, slots_s));
    match sig.results.first() {
        Some(t) if t.is_float() => {
            // movd/movq rax, xmm0: the result's bits, zero-extended.
            a.op(0x66, *t == Type::F64, &[0x0f, 0x7e], 0, RM::Reg(asm::RAX), false, 0);
            a.mov_store(true, asm::RAX, RM::mem(asm::R10, 0));
        }
        Some(_) => a.mov_store(true, asm::RAX, RM::mem(asm::R10, 0)),
        None => {}
    }
    a.op(0, false, &[0x31], asm::RAX, RM::Reg(asm::RAX), false, 0);

    a.bind(common);
    a.mov_load(true, asm::RCX, RM::mem(asm::RBP, ctx_s));
    a.mov_load(true, asm::RDX, RM::mem(asm::RBP, prev_s));
    a.mov_store(true, asm::RDX, RM::mem(asm::RCX, off));
    if abi.win64 {
        for k in 0..10u8 {
            let m = RM::mem(asm::RBP, xmm_base + 16 * k as i32);
            a.op(0xf3, false, &[0x0f, 0x6f], 6 + k, m, false, 0);
        }
    }
    a.op(0, true, &[0x8d], asm::RSP, RM::mem(asm::RBP, -56), false, 0);
    for &g in TRAMP_GPRS.iter().rev() {
        a.pop(g);
    }
    a.pop(asm::RBP);
    a.ret();

    // A trap stub returned here with rsp just above the resume slot and eax = code + 1.
    a.bind(resume);
    a.op(0, true, &[0x8d], asm::RBP, RM::mem(asm::RSP, -resume_s - 8), false, 0);
    a.jmp(common);
    a.finish()?;
    Ok(a.buf)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
