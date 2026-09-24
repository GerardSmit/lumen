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
//!
//! **Handlers.** A region may push and pop `try` handlers of its own (`PushHandler` /
//! `PopHandler` strictly nested inside the loop; the set of region handlers live at each pc is
//! static). The interpreter's handler stack is never touched by native code:
//! - a throw under a region handler does not exit: native code drops the operand entries above
//!   the handler's depth, moves the exception onto the stack and continues at the catch pc (in
//!   the region, or through a resume exit at it when the pad lies outside, like a `for…of`
//!   body's `IterAbortL`);
//! - a resume exit under region handlers stores `1 + k` in `frame.exit_hset`, `k` indexing the
//!   region's handler sets (`Native::hsets`); the interpreter side then steps the ops one at a
//!   time with those handlers installed until the last one is popped (see `finish_handlers`),
//!   so the frame's handler stack is right again when its loop resumes.
//!
//! **External calls.** Import ids below [`EXT_BASE`] are helpers; `EXT_BASE + k` is the `k`-th
//! entry of the region's external addresses (`#[op(fast)]` entries called directly).

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
    /// At a resume exit inside a `try` region the loop itself pushed: 1 + the index of the
    /// region's handler set in [`Native::hsets`] (0 = none). See `enter`.
    pub exit_hset: u64,
    /// Out-parameters of `Helper::TaView`: the viewed bytes' address and element count.
    pub ta_data: *mut u8,
    pub ta_len: usize,
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
pub(crate) const FRAME_EXIT_HSET: i32 = std::mem::offset_of!(JitFrame, exit_hset) as i32;
pub(crate) const FRAME_TA_DATA: i32 = std::mem::offset_of!(JitFrame, ta_data) as i32;
pub(crate) const FRAME_TA_LEN: i32 = std::mem::offset_of!(JitFrame, ta_len) as i32;

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

/// The first import id that names an external address rather than a helper.
pub(crate) const EXT_BASE: u32 = 1 << 16;
/// The import id of the function being compiled itself (a direct recursive call; native
/// targets only, see `build::call`).
pub(crate) const SELF_ID: u32 = EXT_BASE - 1;

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
/// Calls between looks at compiling a whole function (a power of two).
const CALL_MASK: u32 = (1 << 10) - 1;
/// Resume exits at one pc of function code before its speculation there is widened (the
/// function recompiles with the slots involved kept in memory).
const DEOPT_LIMIT: u32 = 32;

/// Function-tier states ([`ChunkJit::fstate`]).
#[allow(dead_code)] // the zero state, spelled for readers
const FS_COLD: u8 = 0;
/// Native code exists: every entry runs it.
const FS_CODE: u8 = 1;
/// Compile at the next entry (after a widening).
const FS_PENDING: u8 = 2;
/// Never compile (unsupported, or out of compiles).
const FS_DEAD: u8 = 3;

/// Per-chunk tier state, stored in [`Chunk`].
#[derive(Default)]
pub(crate) struct ChunkJit {
    ticks: Cell<u32>,
    /// Whether any loop has native code: backward jumps then look the loop up every time.
    has_code: Cell<bool>,
    loops: RefCell<Vec<LoopState>>,
    /// Function entries (the whole-function tier's hotness counter).
    calls: Cell<u32>,
    /// `FS_*`.
    fstate: Cell<u8>,
    func: RefCell<FuncState>,
    /// The direct-call view of the function code (see `build::call`), read by native callers:
    /// the entry address (0 = no code), the frame's shadow-stack size in bytes, and the
    /// [`Native`] (a raw `Rc` pointer, kept alive by `func`).
    dentry: Cell<usize>,
    dsize: Cell<usize>,
    dcode: Cell<usize>,
}

impl ChunkJit {
    fn set_direct(&self, chunk: &Chunk, n: Option<&std::rc::Rc<Native>>) {
        match n {
            Some(n) => {
                self.dsize.set(frame_bytes(chunk.n_slots, n.max_stack));
                self.dcode.set(std::rc::Rc::as_ptr(n) as usize);
                self.dentry.set(n.entry as usize);
            }
            None => self.set_direct_none(),
        }
    }

    fn set_direct_none(&self) {
        self.dentry.set(0);
        self.dsize.set(0);
        self.dcode.set(0);
    }
}

/// Byte offsets of [`ChunkJit`]'s direct-call view inside a [`Chunk`].
pub(crate) const CHUNK_DENTRY: usize =
    std::mem::offset_of!(Chunk, jit) + std::mem::offset_of!(ChunkJit, dentry);
pub(crate) const CHUNK_DSIZE: usize =
    std::mem::offset_of!(Chunk, jit) + std::mem::offset_of!(ChunkJit, dsize);
pub(crate) const CHUNK_DCODE: usize =
    std::mem::offset_of!(Chunk, jit) + std::mem::offset_of!(ChunkJit, dcode);

/// The [`JitFrame`] header of a directly called frame on the shadow stack, rounded so the
/// slots after it are 16-byte aligned.
pub(crate) const FRAME_HDR: usize = (std::mem::size_of::<JitFrame>() + 15) & !15;

/// Shadow-stack bytes of a directly called frame: header, slots, operand-stack area.
pub(crate) fn frame_bytes(n_slots: usize, max_stack: usize) -> usize {
    FRAME_HDR + (n_slots + max_stack.max(1)) * VALUE_SIZE as usize
}

/// The per-thread shadow stack directly called function code runs its frames on (see
/// `build::call`). `top` / `end` are read and bumped by native code; a caller restores `top`
/// when its callee returns, so the stack is always exactly the live direct frames.
///
/// Invariant: every 16-byte-aligned position at or above `top` starts with a byte `<= 4` (a
/// trivially droppable `Value` tag), so a new frame's operand-stack area needs no clearing:
/// the buffer starts zeroed, slots are dropped (or left trivially droppable) when a frame
/// ends, and the caller zeroes the aligned header words it wrote.
#[repr(C)]
pub(crate) struct Shadow {
    pub top: usize,
    pub end: usize,
}

pub(crate) const SHADOW_TOP: i32 = std::mem::offset_of!(Shadow, top) as i32;
pub(crate) const SHADOW_END: i32 = std::mem::offset_of!(Shadow, end) as i32;

/// Shadow-stack bytes per thread.
#[cfg(target_pointer_width = "64")]
const SHADOW_BYTES: usize = 4 << 20;
#[cfg(target_pointer_width = "32")]
const SHADOW_BYTES: usize = 1 << 20;

thread_local! {
    static SHADOW: Cell<*mut Shadow> = const { Cell::new(std::ptr::null_mut()) };
}

/// This thread's shadow stack (allocated on first use and never freed: code compiled on a
/// thread bakes the address in, and may outlive the thread — a parked coroutine worker's code
/// runs on its resumer). Only one thread runs JS of an engine at a time, and every use of a
/// shadow stack is strictly nested, so frames of code from different threads never interleave
/// out of order.
pub(crate) fn shadow() -> *mut Shadow {
    SHADOW.with(|s| {
        if s.get().is_null() {
            let buf: &'static mut [u128] = Box::leak(vec![0u128; SHADOW_BYTES / 16].into_boxed_slice());
            let base = buf.as_mut_ptr() as usize;
            s.set(Box::into_raw(Box::new(Shadow {
                top: base,
                end: base + SHADOW_BYTES,
            })));
        }
        s.get()
    })
}

/// The code `entry` of `chunk`'s function tier (current or retired), for a direct call's exit.
fn find_native(chunk: &Chunk, entry: usize) -> Option<std::rc::Rc<Native>> {
    let fs = chunk.jit.func.borrow();
    fs.code
        .iter()
        .chain(fs.retired.iter())
        .find(|n| n.entry as usize == entry)
        .cloned()
}

/// The whole-function tier of one chunk: native code entered at pc 0 by [`on_entry`].
#[derive(Default)]
struct FuncState {
    code: Option<std::rc::Rc<Native>>,
    compiles: u32,
    entry_fails: u32,
    /// Slots kept Boxed on the next compile (their speculated kind failed).
    widen: Vec<bool>,
    /// Resume exits per pc since the last compile, `(pc, count)`.
    deopts: Vec<(u32, u32)>,
    /// Retired code (see [`retire_fn`]).
    retired: Vec<std::rc::Rc<Native>>,
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
    /// Region handler sets `(catch_pc, stack_depth)`, outermost first (see `exit_hset`).
    hsets: Vec<Vec<(usize, usize)>>,
    /// Values whose identity the code compares against (callees of direct fast calls): held so
    /// their addresses cannot be reused by another object while the code lives.
    _pins: Vec<Value>,
    /// Heap cells whose addresses the code embeds (pinned callee `Value`s and environments of
    /// direct call sites, see `build::call`).
    _boxes: Vec<Box<dyn std::any::Any>>,
    /// The representation of each slot the code was compiled for (entry guards).
    kinds: Vec<Kind>,
    /// Start pcs of call sites guarded at every call (see [`build::Built::guarded`]).
    guarded: Vec<usize>,
}

fn log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_TIER_LOG").is_some())
}

/// `LUMEN_JIT_DUMP`: print each compiled unit's optimized IR (debugging).
fn dump_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_JIT_DUMP").is_some())
}

fn dump_ops(chunk: &Chunk) {
    if dump_enabled() {
        for (pc, op) in chunk.ops.iter().enumerate() {
            eprintln!("  {pc:4} {op:?}");
        }
    }
}

fn eager() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_JIT_EAGER").is_some())
}

fn jit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let target = cfg!(target_arch = "x86_64")
            || cfg!(all(target_arch = "aarch64", not(target_os = "ios")))
            || (cfg!(target_arch = "wasm32") && crate::WASM_JIT_HOST.get().is_some());
        // wasm32 has no process environment worth reading (`var_os` is always `None` there).
        target && std::env::var_os("LUMEN_NO_JIT").is_none()
    })
}

/// Compile `[header, backedge]` against the current slot kinds.
fn compile(
    i: &Interp,
    env: &Env,
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
    this_val: &Value,
) -> Result<Native, String> {
    dump_ops(chunk);
    let built = build::build(i, env, chunk, header, backedge, slots, this_val)?;
    finish(built, header, backedge)
}

/// Compile the whole of `chunk`, entered at pc 0 with the current `slots` (see [`on_entry`]).
fn compile_fn(
    i: &Interp,
    env: &Env,
    chunk: &Chunk,
    slots: &[Value],
    widen: &[bool],
    this_val: &Value,
) -> Result<Native, String> {
    dump_ops(chunk);
    let built = build::build_fn(i, env, chunk, slots, widen, this_val)?;
    finish(built, 0, chunk.ops.len().saturating_sub(1))
}

fn finish(built: build::Built, header: usize, backedge: usize) -> Result<Native, String> {
    let mut func = built.func;
    // Cheap next to compilation, and a malformed function must never reach the backend.
    lumen_codegen::verify::verify(&func).map_err(|e| format!("verify: {e}"))?;
    lumen_codegen::opt::optimize(&mut func);
    if dump_enabled() {
        eprintln!("{func}");
    }
    let (keep, entry) = emit(&func, &built.externs, header, backedge)?;
    if built.self_entry != 0 {
        // SAFETY: `self_entry` is the address of a `Cell<usize>` owned by `built.boxes`.
        unsafe { (*(built.self_entry as *const Cell<usize>)).set(entry as usize) };
    }
    Ok(Native {
        _keep: keep,
        entry,
        max_stack: built.max_stack,
        hsets: built.hsets,
        _pins: built.pins,
        _boxes: built.boxes,
        kinds: built.kinds,
        guarded: built.guarded,
    })
}

/// The address of import `id`: a helper, or one of the region's external addresses.
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "wasm32",
        all(target_arch = "aarch64", not(target_os = "ios"))
    )),
    allow(dead_code)
)]
fn import_address(externs: &[u64], id: u32) -> Option<u64> {
    if id >= EXT_BASE {
        externs.get((id - EXT_BASE) as usize).copied()
    } else {
        helpers::address(id)
    }
}

/// x86-64 and AArch64: machine code in executable memory. iOS forbids runtime code generation,
/// so it keeps the interpreter (and later, ahead-of-time code in `.text`).
#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "aarch64", not(target_os = "ios"))
))]
fn emit(
    func: &lumen_codegen::Function,
    externs: &[u64],
    header: usize,
    backedge: usize,
) -> Result<(Box<dyn std::any::Any>, NativeFn), String> {
    #[cfg(target_arch = "x86_64")]
    use lumen_codegen::x64 as native;
    #[cfg(target_arch = "aarch64")]
    use lumen_codegen::aarch64 as native;
    let cfg = native::Config::host(None);
    let compiled = native::compile(func, &cfg)?;
    let (mem, addrs) = native::load(&[compiled], |id, addrs| {
        if id == SELF_ID {
            addrs.first().copied()
        } else {
            import_address(externs, id)
        }
    })?;
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
    externs: &[u64],
    _header: usize,
    _backedge: usize,
) -> Result<(Box<dyn std::any::Any>, NativeFn), String> {
    let cfg = lumen_codegen::wasm::Config { ptr32: true };
    let bytes = lumen_codegen::wasm::compile_module(std::slice::from_ref(func), &cfg, |id| {
        import_address(externs, id).map(|a| a as u32)
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
    _externs: &[u64],
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
            match compile(i, env, chunk, header, backedge, slots, this_val) {
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
    enter(i, chunk, env, slots, stack, pc, this_val, Some((header, backedge)), &native)
}

/// The interpreter's inline check at a frame's entry (pc 0, empty operand stack): count the
/// call and report whether [`on_entry`] has anything to do (function code to run, or a look at
/// compiling one: every `CALL_MASK + 1` calls, after a loop of the chunk compiled, after a
/// widening, or always under `LUMEN_JIT_EAGER`).
#[inline(always)]
pub(super) fn entry_due(chunk: &Chunk) -> bool {
    let jit = &chunk.jit;
    let st = jit.fstate.get();
    if st == FS_CODE {
        return true;
    }
    let c = jit.calls.get().wrapping_add(1);
    jit.calls.set(c);
    st != FS_DEAD && (c & CALL_MASK == 0 || st == FS_PENDING || jit.has_code.get() || eager())
}

/// The hook at a frame's entry: `*pc == 0`, `stack` empty, no handlers. Runs the chunk's
/// function code (compiling it first when due). Returns like [`on_backedge`]: `Ok(None)` to keep
/// interpreting at `*pc` (moved when the code exited part-way, with the materialized operand
/// stack), `Ok(Some(step))` when the function returned, or a throw.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(super) fn on_entry(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
) -> Result<Option<VmStep>, Abrupt> {
    let jit = &chunk.jit;
    let native = match jit.fstate.get() {
        FS_CODE => match jit.func.borrow().code.clone() {
            Some(n) => n,
            None => return Ok(None),
        },
        FS_DEAD => return Ok(None),
        _ => {
            if !jit_enabled() || *pc != 0 || !stack.is_empty() {
                return Ok(None);
            }
            let mut fs = jit.func.borrow_mut();
            if fs.compiles >= MAX_COMPILES {
                jit.fstate.set(FS_DEAD);
                return Ok(None);
            }
            fs.compiles += 1;
            match compile_fn(i, env, chunk, slots, &fs.widen, this_val) {
                Ok(n) => {
                    if log_enabled() {
                        eprintln!(
                            "[jit] compiled function ({} ops, compile {})",
                            chunk.ops.len(),
                            fs.compiles
                        );
                    }
                    let n = std::rc::Rc::new(n);
                    jit.set_direct(chunk, Some(&n));
                    fs.code = Some(n.clone());
                    fs.entry_fails = 0;
                    fs.deopts.clear();
                    jit.fstate.set(FS_CODE);
                    n
                }
                Err(e) => {
                    if log_enabled() {
                        eprintln!("[jit] function ({} ops) not compiled: {e}", chunk.ops.len());
                    }
                    jit.fstate.set(FS_DEAD);
                    return Ok(None);
                }
            }
        }
    };
    enter(i, chunk, env, slots, stack, pc, this_val, None, &native)
}

/// The function code of `chunk` when [`helpers`]' direct native call can run it: compiled,
/// with its slots and operand stack small enough for the Rust stack.
#[inline(always)]
fn native_code(chunk: &Chunk) -> Option<std::rc::Rc<Native>> {
    if chunk.jit.fstate.get() != FS_CODE || chunk.n_slots > helpers::NATIVE_SLOTS {
        return None;
    }
    chunk.jit.func.borrow().code.clone()
}

/// The JIT's call helper's shortcut for a frame it just entered (pc 0, empty stack, no
/// handlers): run the chunk's function code without the interpreter's driver. `None` when there
/// is none, or when it exited part-way (the driver continues at `*pc`).
#[inline(always)]
fn run_entered(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
) -> Option<Result<VmStep, Abrupt>> {
    if chunk.jit.fstate.get() != FS_CODE {
        return None;
    }
    match on_entry(i, chunk, env, slots, stack, pc, this_val) {
        Ok(Some(step)) => Some(Ok(step)),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

/// Operand-stack entries a native entry provides on the Rust stack before falling back to a
/// heap buffer.
const INLINE_AREA: usize = 32;

/// Run `native` against the frame: a loop region `Some((header, backedge))` entered at its
/// header, or the chunk's function code (`None`) entered at pc 0.
#[allow(clippy::too_many_arguments)]
fn enter(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    region: Option<(usize, usize)>,
    native: &Native,
) -> Result<Option<VmStep>, Abrupt> {
    let n = native.max_stack.max(1);
    // Only the `n` entries the code uses are initialized (and dropped, by `_own`).
    let mut small = [const { std::mem::MaybeUninit::<Value>::uninit() }; INLINE_AREA];
    let mut big: Vec<Value>;
    let area: &mut [Value] = if n <= INLINE_AREA {
        for v in &mut small[..n] {
            v.write(Value::Undefined);
        }
        // SAFETY: the first `n` entries were just initialized.
        unsafe { std::slice::from_raw_parts_mut(small.as_mut_ptr().cast::<Value>(), n) }
    } else {
        big = Vec::new();
        big.resize(n, Value::Undefined);
        &mut big[..]
    };
    struct Own(*mut Value, usize);
    impl Drop for Own {
        fn drop(&mut self) {
            // SAFETY: `small[..n]` is initialized and dropped exactly once, here.
            unsafe { std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(self.0, self.1)) }
        }
    }
    let _own = (n <= INLINE_AREA).then(|| Own(area.as_mut_ptr(), n));
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
        exit_hset: 0,
        ta_data: std::ptr::null_mut(),
        ta_len: 0,
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
            match region {
                Some((header, backedge)) => {
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
                }
                None => fn_entry_failed(chunk, slots, native),
            }
            Ok(None)
        }
        EXIT_RETURN => {
            let v = if depth >= 1 {
                std::mem::replace(&mut area[0], Value::Undefined)
            } else {
                Value::Undefined
            };
            Ok(Some(VmStep::Done(v)))
        }
        _ => {
            stack.extend(
                area[..depth]
                    .iter_mut()
                    .map(|v| std::mem::replace(v, Value::Undefined)),
            );
            *pc = exit_pc;
            if kind == EXIT_THROW {
                return Err(Abrupt::Throw(exception));
            }
            match region {
                None => fn_deopt(chunk, exit_pc, native),
                // A guarded call site stopped holding (see `Helper::SlotGuard`): recompile the
                // loop without it rather than exit there on every iteration.
                Some((header, backedge))
                    if kind == EXIT_RESUME && native.guarded.contains(&exit_pc) =>
                {
                    let mut loops = chunk.jit.loops.borrow_mut();
                    if let Some(l) = loops
                        .iter_mut()
                        .find(|l| l.header as usize == header && l.backedge as usize == backedge)
                    {
                        l.code = None;
                    }
                }
                Some(_) => {}
            }
            match (frame.exit_hset as usize)
                .checked_sub(1)
                .and_then(|k| native.hsets.get(k))
            {
                Some(set) => finish_handlers(i, chunk, env, slots, stack, pc, this_val, set),
                None => Ok(None),
            }
        }
    }
}

/// Function code failed its entry guards (an argument of another kind): after enough failures,
/// recompile with the mismatching slots kept Boxed.
#[cold]
fn fn_entry_failed(chunk: &Chunk, slots: &[Value], native: &Native) {
    let jit = &chunk.jit;
    let mut fs = jit.func.borrow_mut();
    fs.entry_fails += 1;
    if fs.entry_fails < MAX_ENTRY_FAILS {
        return;
    }
    let n = native.kinds.len();
    if fs.widen.len() < n {
        fs.widen.resize(n, false);
    }
    for (s, &k) in native.kinds.iter().enumerate() {
        if k != Kind::Boxed && slots.get(s).is_some_and(|v| Kind::of(v) != k) {
            fs.widen[s] = true;
        }
    }
    retire_fn(jit, &mut fs);
}

/// A resume exit of function code at `pc` (the interpreter finishes the call). Exits that keep
/// happening at one pc widen the slots its op (or the op before, for exits after an op) uses and
/// recompile; exits no widening can remove (ops the translator leaves to the interpreter) are
/// just counted.
fn fn_deopt(chunk: &Chunk, pc: usize, native: &Native) {
    let jit = &chunk.jit;
    let mut fs = jit.func.borrow_mut();
    if native.guarded.contains(&pc) && fs.code.as_ref().is_some_and(|c| std::ptr::eq(&**c, native)) {
        // A guarded call site stopped holding: recompile without it.
        retire_fn(jit, &mut fs);
        return;
    }
    let count = match fs.deopts.iter_mut().find(|d| d.0 as usize == pc) {
        Some(d) => {
            d.1 = d.1.saturating_add(1);
            d.1
        }
        None => {
            if fs.deopts.len() >= 64 {
                return;
            }
            fs.deopts.push((pc as u32, 1));
            1
        }
    };
    if count != DEOPT_LIMIT {
        return;
    }
    let n = native.kinds.len();
    if fs.widen.len() < n {
        fs.widen.resize(n, false);
    }
    let mut widened = false;
    for q in [Some(pc), pc.checked_sub(1)].into_iter().flatten() {
        let Some(op) = chunk.ops.get(q) else { continue };
        for s in build::op_slots(op) {
            let s = s as usize;
            if native.kinds.get(s).is_some_and(|&k| k != Kind::Boxed) && !fs.widen[s] {
                fs.widen[s] = true;
                widened = true;
            }
        }
    }
    if widened {
        if log_enabled() {
            eprintln!("[jit] function deopts at {pc}: recompiling with wider kinds");
        }
        retire_fn(jit, &mut fs);
    }
}

/// Drop the function code: recompile at the next entry, or stay interpreted when out of
/// compiles.
fn retire_fn(jit: &ChunkJit, fs: &mut FuncState) {
    // Directly called frames of the code may still be running (a recursion deopting in its
    // innermost frame): keep the code alive with the chunk (at most `MAX_COMPILES` of them).
    jit.set_direct_none();
    if let Some(old) = fs.code.take() {
        fs.retired.push(old);
    }
    fs.entry_fails = 0;
    fs.deopts.clear();
    jit.fstate.set(if fs.compiles >= MAX_COMPILES {
        FS_DEAD
    } else {
        FS_PENDING
    });
}

/// A resume exit inside `try` regions the loop pushed itself (`set`, outermost first): the
/// interpreter's handler stack does not have them, so run the ops one at a time here with them
/// installed — unwinding to them on a throw exactly as `drive_vm` would — until the last one is
/// popped (or the function returns); then the caller's loop continues with its own handlers.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn finish_handlers(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    set: &[(usize, usize)],
) -> Result<Option<VmStep>, Abrupt> {
    let mut hs: Vec<super::Handler> = set
        .iter()
        .map(|&(catch_pc, stack_depth)| super::Handler {
            catch_pc,
            stack_depth,
        })
        .collect();
    while !hs.is_empty() {
        match super::run_vm::<true>(i, chunk, env, slots, stack, pc, this_val, &mut hs) {
            // One op ran (`Done(Empty)` is the single-step marker; a real completion is never
            // `Empty`).
            Ok(VmStep::Done(Value::Empty)) => {}
            Ok(step) => return Ok(Some(step)),
            Err(Abrupt::Throw(e)) => {
                let h = hs.pop().expect("non-empty");
                stack.truncate(h.stack_depth);
                stack.push(e);
                *pc = h.catch_pc;
            }
            Err(other) => return Err(other),
        }
    }
    Ok(None)
}
