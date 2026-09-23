//! The optimizing tier: hot loops of a bytecode [`Chunk`] compile to native code through
//! `lumen-codegen`, entered by on-stack replacement at the loop header and left — normally, on a
//! failed speculation, or on a throw — by returning an exit word to the interpreter, which
//! resumes at the exit's pc with the operand stack the native code materialized. The
//! interpreter is always the fallback: native code never needs to handle a case it did not
//! speculate on; it exits and lets `run_vm` run the op.
//!
//! # Contract (shared by [`build`], [`helpers`] and [`layout`])
//!
//! **Region.** A loop `[header, backedge]`: `header` is the target of the backward `Jump` at
//! `backedge`. The interpreter's operand stack is empty at `header` when the region is entered.
//! Every jump leaving `[header, backedge]` (and falling through past `backedge`) is a region
//! exit: native code writes back its state and returns `exit(pc, EXIT_RESUME)`.
//!
//! **Native signature.** `extern "C" fn(frame: *mut JitFrame) -> u64` — IR signature
//! `(PTR) -> I64`; [`PTR`] is the target's pointer type (I64, or I32 on wasm32 — where the
//! "native" code is a WebAssembly module sharing the engine's memory and function table). No traps, no stack-limit check: every failure is a returned exit word.
//!
//! **Exit word.** `(pc << 8) | kind`, see the `EXIT_*` constants. Before returning, native code
//! 1. writes every local it holds in SSA back to `slots` (a `Num` slot as tag 4 + f64 payload,
//!    a `Bool` slot as tag 3 + byte), and
//! 2. materializes the abstract operand stack into `frame.stack[0..depth]` and stores `depth` in
//!    `frame.exit_depth`. The interpreter then moves those values onto its own stack.
//!
//! **Values in memory.** A [`Value`] is 16 bytes, `repr(u8)`: tag byte at offset 0
//! (`TAG_*`), `Bool` byte at offset 1, `Num` f64 / pointer payload at offset 8.
//!
//! **Local kinds.** Chosen at compile time from the slot's value at OSR time ([`Kind`]):
//! `Num` and `Bool` locals live in SSA (IR variables, F64 / I32) and are guarded at region entry
//! (a mismatch returns `EXIT_ENTRY_FAIL` before anything happened); `Boxed` locals stay in
//! `slots` and are read and written through helpers (they may hold refcounted values).
//!
//! **Operand stack.** Abstract stack entry at depth `d` is either an unboxed SSA value (`Num`
//! F64, `Bool` I32) or `Boxed`: an owned `Value` living *in place* at `frame.stack[d]`. Invariant:
//! every `frame.stack[d]` that is not a live Boxed entry holds a trivially droppable value
//! (tag <= 4), so native code may overwrite it with plain stores; helpers that consume a Boxed
//! operand take it out (leaving `Undefined`). Materializing a Boxed entry is therefore free;
//! an unboxed one is stored (tag + payload) at `frame.stack[d]`.
//!
//! **Helpers** (`extern "C"`, see [`helpers`]) take the frame first and return a status:
//! `STATUS_OK`, or `STATUS_THROW` with the exception in `frame.exception` — native code then
//! materializes its state and returns `exit(pc_of_the_op, EXIT_THROW)`.

pub(super) mod build;
pub(super) mod helpers;
pub(super) mod layout;

use super::Chunk;
use crate::interpreter::{Env, Interp};
use crate::value::Value;

/// The frame native region code runs against. Offsets are part of the contract (`FRAME_*`).
#[repr(C)]
pub(crate) struct JitFrame {
    /// The chunk's frame slots (`&mut [Value]`, `chunk.n_slots` long).
    pub slots: *mut Value,
    /// `chunk.consts`.
    pub consts: *const Value,
    /// The region's operand-stack area, `max_stack` Values, all `Undefined` on entry.
    pub stack: *mut Value,
    pub interp: *mut Interp,
    pub chunk: *const Chunk,
    pub env: *const Env,
    pub this_val: *const Value,
    /// Live entries of `stack` at exit (written by native code).
    pub exit_depth: u64,
    /// The pending exception after a helper returned `STATUS_THROW`.
    pub exception: Value,
    /// Loop turns left before the next safepoint: native code decrements it at every backedge
    /// and calls `Helper::Safepoint` when it drops to zero or below (GC and interrupts, like the
    /// interpreter's amortized check at backward jumps). The helper resets it.
    pub budget: i64,
}

// Offsets differ between 64-bit hosts and wasm32 (4-byte pointers), so they are computed.
pub(crate) const FRAME_SLOTS: i32 = std::mem::offset_of!(JitFrame, slots) as i32;
pub(crate) const FRAME_CONSTS: i32 = std::mem::offset_of!(JitFrame, consts) as i32;
pub(crate) const FRAME_STACK: i32 = std::mem::offset_of!(JitFrame, stack) as i32;
pub(crate) const FRAME_INTERP: i32 = std::mem::offset_of!(JitFrame, interp) as i32;
pub(crate) const FRAME_CHUNK: i32 = std::mem::offset_of!(JitFrame, chunk) as i32;
pub(crate) const FRAME_ENV: i32 = std::mem::offset_of!(JitFrame, env) as i32;
pub(crate) const FRAME_THIS: i32 = std::mem::offset_of!(JitFrame, this_val) as i32;
pub(crate) const FRAME_EXIT_DEPTH: i32 = std::mem::offset_of!(JitFrame, exit_depth) as i32;
pub(crate) const FRAME_EXCEPTION: i32 = std::mem::offset_of!(JitFrame, exception) as i32;
pub(crate) const FRAME_BUDGET: i32 = std::mem::offset_of!(JitFrame, budget) as i32;

/// The IR type of a pointer on this target: I64 on 64-bit hosts, I32 on wasm32. Every address
/// (the frame parameter, loaded pointers, helper pointer arguments) uses it.
#[cfg(target_pointer_width = "64")]
pub(crate) const PTR: lumen_codegen::Type = lumen_codegen::Type::I64;
#[cfg(target_pointer_width = "32")]
pub(crate) const PTR: lumen_codegen::Type = lumen_codegen::Type::I32;
/// The memory access loading or storing a pointer.
#[cfg(target_pointer_width = "64")]
pub(crate) const PTR_MEM: lumen_codegen::MemKind = lumen_codegen::MemKind::I64;
#[cfg(target_pointer_width = "32")]
pub(crate) const PTR_MEM: lumen_codegen::MemKind = lumen_codegen::MemKind::I32;
/// The budget [`JitFrame::budget`] is (re)set to.
pub(crate) const SAFEPOINT_BUDGET: i64 = 1 << 12;

const _: () = assert!(std::mem::size_of::<Value>() == VALUE_SIZE as usize);

/// `size_of::<Value>()`.
pub(crate) const VALUE_SIZE: i32 = 16;
/// Offset of the `Bool` byte in a `Value`.
pub(crate) const VALUE_BOOL: i32 = 1;
/// Offset of the f64 / pointer payload in a `Value`.
pub(crate) const VALUE_PAYLOAD: i32 = 8;

pub(crate) const TAG_UNDEFINED: u8 = 0;
pub(crate) const TAG_EMPTY: u8 = 1;
pub(crate) const TAG_NULL: u8 = 2;
pub(crate) const TAG_BOOL: u8 = 3;
pub(crate) const TAG_NUM: u8 = 4;
pub(crate) const TAG_BIGINT: u8 = 5;
pub(crate) const TAG_STR: u8 = 6;
pub(crate) const TAG_SYM: u8 = 7;
pub(crate) const TAG_OBJ: u8 = 8;

/// Resume the interpreter at the exit pc with the materialized stack.
pub(crate) const EXIT_RESUME: u64 = 0;
/// Throw `frame.exception` at the exit pc (after pushing the materialized stack).
pub(crate) const EXIT_THROW: u64 = 1;
/// Return `frame.stack[0]` from the function (`exit_depth == 1`).
pub(crate) const EXIT_RETURN: u64 = 2;
/// An entry guard failed; nothing ran. The interpreter continues at the header.
pub(crate) const EXIT_ENTRY_FAIL: u64 = 3;

pub(crate) const fn exit(pc: usize, kind: u64) -> u64 {
    ((pc as u64) << 8) | kind
}

pub(crate) const STATUS_OK: u32 = 0;
pub(crate) const STATUS_THROW: u32 = 1;

/// How a local (or an operand-stack entry) is represented in native code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Unboxed F64 in SSA.
    Num,
    /// Unboxed I32 0/1 in SSA.
    Bool,
    /// An owned `Value` in memory (a slot, or `frame.stack[d]`).
    Boxed,
}

impl Kind {
    pub(crate) fn of(v: &Value) -> Kind {
        match v {
            Value::Num(_) => Kind::Num,
            Value::Bool(_) => Kind::Bool,
            _ => Kind::Boxed,
        }
    }
}

pub(crate) type NativeFn = unsafe extern "C" fn(*mut JitFrame) -> u64;

// ---- the interpreter side: hotness, compilation cache, entry and exit ----------------------

use super::VmStep;
use crate::interpreter::Abrupt;
use std::cell::{Cell, RefCell};

/// Backward jumps a chunk takes between looks at its loops (a power of two).
const TICK_MASK: u32 = (1 << 10) - 1;
/// Entry-guard failures (a local changed kind) before a loop is recompiled for the new kinds.
const MAX_ENTRY_FAILS: u32 = 16;
/// Compilations a loop gets before it stays interpreted.
const MAX_COMPILES: u32 = 3;

/// Per-chunk tier state, stored in [`Chunk`].
#[derive(Default)]
pub(crate) struct ChunkJit {
    ticks: Cell<u32>,
    /// Whether any loop has native code: backward jumps then look the loop up every time.
    has_code: Cell<bool>,
    loops: RefCell<Vec<LoopState>>,
}

struct LoopState {
    header: u32,
    backedge: u32,
    code: Option<std::rc::Rc<Native>>,
    compiles: u32,
    entry_fails: u32,
}

struct Native {
    /// Keeps the code alive (executable memory on native hosts; nothing on wasm32, where the
    /// instantiated module lives in the engine's function table).
    _keep: Box<dyn std::any::Any>,
    entry: NativeFn,
    max_stack: usize,
}

fn log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_TIER_LOG").is_some())
}

fn eager() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_JIT_EAGER").is_some())
}

fn jit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let target = cfg!(target_arch = "x86_64")
            || (cfg!(target_arch = "wasm32") && crate::WASM_JIT_HOST.get().is_some());
        // wasm32 has no process environment worth reading (`var_os` is always `None` there).
        target && std::env::var_os("LUMEN_NO_JIT").is_none()
    })
}

/// Compile `[header, backedge]` against the current slot kinds.
fn compile(chunk: &Chunk, header: usize, backedge: usize, slots: &[Value]) -> Result<Native, String> {
    let built = build::build(chunk, header, backedge, slots)?;
    let mut func = built.func;
    // Cheap next to compilation, and a malformed function must never reach the backend.
    lumen_codegen::verify::verify(&func).map_err(|e| format!("verify: {e}"))?;
    lumen_codegen::opt::optimize(&mut func);
    let (keep, entry) = emit(&func, header, backedge)?;
    Ok(Native {
        _keep: keep,
        entry,
        max_stack: built.max_stack,
    })
}

/// x86-64 and AArch64: machine code in executable memory. iOS forbids runtime code generation,
/// so it keeps the interpreter (and later, ahead-of-time code in `.text`).
#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "aarch64", not(target_os = "ios"))
))]
fn emit(
    func: &lumen_codegen::Function,
    header: usize,
    backedge: usize,
) -> Result<(Box<dyn std::any::Any>, NativeFn), String> {
    #[cfg(target_arch = "x86_64")]
    use lumen_codegen::x64 as native;
    #[cfg(target_arch = "aarch64")]
    use lumen_codegen::aarch64 as native;
    let cfg = native::Config::host(None);
    let compiled = native::compile(func, &cfg)?;
    let (mem, addrs) = native::load(&[compiled], |id, _| helpers::address(id))?;
    if let Some(dir) = std::env::var_os("LUMEN_JIT_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("loop_{header}_{backedge}.bin"));
        // SAFETY: `mem` holds `mem.len()` initialized bytes.
        let bytes = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem.len()) };
        let _ = std::fs::write(path, bytes);
    }
    // SAFETY: the code was generated for exactly this signature.
    let entry = unsafe { std::mem::transmute::<usize, NativeFn>(addrs[0] as usize) };
    Ok((Box::new(mem), entry))
}

/// wasm32: a WebAssembly module over the engine's own memory and function table, instantiated
/// by the embedder's [`crate::set_wasm_jit_host`]; the entry is the table index it returns (on
/// wasm32 a function pointer *is* a table index, called with a type-checked `call_indirect`).
/// Helpers are called the same way: their Rust function pointers are their table indices.
#[cfg(target_arch = "wasm32")]
fn emit(
    func: &lumen_codegen::Function,
    _header: usize,
    _backedge: usize,
) -> Result<(Box<dyn std::any::Any>, NativeFn), String> {
    let cfg = lumen_codegen::wasm::Config { ptr32: true };
    let bytes = lumen_codegen::wasm::compile_module(std::slice::from_ref(func), &cfg, |id| {
        helpers::address(id).map(|a| a as u32)
    })?;
    let host = crate::WASM_JIT_HOST.get().ok_or("no wasm JIT host")?;
    let index = host(&bytes).ok_or("the wasm JIT host did not instantiate the module")?;
    // SAFETY: the table entry is the module's `f0`, of wasm type (i32) -> i64 — exactly
    // `NativeFn` on wasm32.
    let entry = unsafe { std::mem::transmute::<usize, NativeFn>(index as usize) };
    Ok((Box::new(()), entry))
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "wasm32",
    all(target_arch = "aarch64", not(target_os = "ios"))
)))]
fn emit(
    _func: &lumen_codegen::Function,
    _header: usize,
    _backedge: usize,
) -> Result<(Box<dyn std::any::Any>, NativeFn), String> {
    Err("no JIT backend for this target".into())
}

/// The interpreter's inline per-backedge check: bump the chunk's tick counter and report whether
/// [`on_backedge`] has anything to do (native code exists, or a tick boundary to look for a
/// compile). Keeps the out-of-line hook's call off the interpreter's loop path.
#[inline(always)]
pub(super) fn backedge_due(chunk: &Chunk) -> bool {
    let jit = &chunk.jit;
    let t = jit.ticks.get().wrapping_add(1);
    jit.ticks.set(t);
    t & TICK_MASK == 0 || jit.has_code.get() || eager()
}

/// The hook at a backward `Jump` from `backedge` to `header`; `*pc == header` (the interpreter
/// is about to continue there). Returns `Ok(None)` to keep interpreting at `*pc` (which moved
/// if native code ran part of the loop), `Ok(Some(step))` when the function returned, or a throw.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
#[cold]
pub(super) fn on_backedge(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    backedge: usize,
) -> Result<Option<VmStep>, Abrupt> {
    let header = *pc;
    let jit = &chunk.jit;
    // `LUMEN_JIT_EAGER` compiles at the first backedge (stress testing: every loop runs
    // natively as soon as possible).
    let t = if eager() { 0 } else { jit.ticks.get() };
    if !stack.is_empty() || !jit_enabled() {
        return Ok(None);
    }
    let native = {
        let mut loops = jit.loops.borrow_mut();
        let idx = match loops
            .iter()
            .position(|l| l.header as usize == header && l.backedge as usize == backedge)
        {
            Some(k) => k,
            None => {
                loops.push(LoopState {
                    header: header as u32,
                    backedge: backedge as u32,
                    code: None,
                    compiles: 0,
                    entry_fails: 0,
                });
                loops.len() - 1
            }
        };
        let l = &mut loops[idx];
        if l.code.is_none() {
            // Not compiled: only look again on a tick boundary.
            if l.compiles >= MAX_COMPILES || t & TICK_MASK != 0 {
                return Ok(None);
            }
            l.compiles += 1;
            match compile(chunk, header, backedge, slots) {
                Ok(n) => {
                    if log_enabled() {
                        eprintln!("[jit] compiled loop {header}..={backedge}");
                    }
                    l.code = Some(std::rc::Rc::new(n));
                    l.entry_fails = 0;
                    jit.has_code.set(true);
                }
                Err(e) => {
                    if log_enabled() {
                        eprintln!("[jit] loop {header}..={backedge} not compiled: {e}");
                    }
                    l.compiles = MAX_COMPILES;
                    return Ok(None);
                }
            }
        }
        l.code.clone().expect("compiled above")
    };
    enter(i, chunk, env, slots, stack, pc, this_val, header, backedge, &native)
}

#[allow(clippy::too_many_arguments)]
fn enter(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    header: usize,
    backedge: usize,
    native: &Native,
) -> Result<Option<VmStep>, Abrupt> {
    let mut area: Vec<Value> = Vec::new();
    area.resize(native.max_stack.max(1), Value::Undefined);
    let mut frame = JitFrame {
        slots: slots.as_mut_ptr(),
        consts: chunk.consts.as_ptr(),
        stack: area.as_mut_ptr(),
        interp: i as *mut Interp,
        chunk: chunk as *const Chunk,
        env: env as *const Env,
        this_val: this_val as *const Value,
        exit_depth: 0,
        exception: Value::Undefined,
        budget: SAFEPOINT_BUDGET,
    };
    // SAFETY: the frame points at live state for the whole call; native code follows the
    // contract above (it touches only `slots`, `area` and the frame, and calls helpers).
    let word = unsafe { (native.entry)(&mut frame) };
    let kind = word & 0xff;
    let exit_pc = (word >> 8) as usize;
    let depth = (frame.exit_depth as usize).min(area.len());
    let exception = std::mem::take(&mut frame.exception);
    match kind {
        EXIT_ENTRY_FAIL => {
            let mut loops = chunk.jit.loops.borrow_mut();
            if let Some(l) = loops
                .iter_mut()
                .find(|l| l.header as usize == header && l.backedge as usize == backedge)
            {
                l.entry_fails += 1;
                if l.entry_fails >= MAX_ENTRY_FAILS {
                    // The locals' kinds moved on: recompile at the next tick boundary.
                    l.code = None;
                }
            }
            Ok(None)
        }
        EXIT_RETURN => {
            let v = if depth >= 1 {
                std::mem::take(&mut area[0])
            } else {
                Value::Undefined
            };
            Ok(Some(VmStep::Done(v)))
        }
        _ => {
            stack.extend(area.drain(..depth));
            *pc = exit_pc;
            if kind == EXIT_THROW {
                Err(Abrupt::Throw(exception))
            } else {
                Ok(None)
            }
        }
    }
}
