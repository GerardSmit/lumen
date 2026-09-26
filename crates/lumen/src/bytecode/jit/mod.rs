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
///
/// On 64-bit hosts every 16-byte-aligned field holds a value whose first byte stays at most 4
/// on the returning path of a direct call (a tag of a taken exception, a budget of `1 << 12`,
/// an exit depth of 0 or 1, a handler-set index of 0, never-written padding), so a directly
/// called frame's header keeps the shadow stack's invariant (see [`Shadow`]) without clearing.
#[repr(C)]
pub(crate) struct JitFrame {
    /// The pending exception after a helper returned `STATUS_THROW`.
    pub exception: Value,
    /// Loop turns left before the next safepoint: native code decrements it at every backedge
    /// and calls `Helper::Safepoint` when it drops to zero or below (GC and interrupts, like the
    /// interpreter's amortized check at backward jumps). The helper resets it. (Read once at
    /// entry: native code counts in a variable.)
    pub budget: i64,
    /// The chunk's frame slots (`&mut [Value]`, `chunk.n_slots` long).
    pub slots: *mut Value,
    /// Live entries of `stack` at exit (written by native code).
    pub exit_depth: u64,
    /// `chunk.consts`.
    pub consts: *const Value,
    /// At a resume exit inside a `try` region the loop itself pushed: 1 + the index of the
    /// region's handler set in [`Native::hsets`] (0 = none). See `enter`.
    pub exit_hset: u64,
    /// The region's operand-stack area, `max_stack` Values, all `Undefined` on entry.
    pub stack: *mut Value,
    /// The running activation's engine call flags as a direct call site needs them (see
    /// `build::call`'s `entry_flags`): 1 + `Interp::strict` when they are canonical, else 0.
    /// Written by [`enter`] and by every direct call.
    pub canon: u64,
    pub interp: *mut Interp,
    /// Which entry of function code to take: 0 = pc 0, `k` = the `k`-th resume point after an
    /// `await` ([`Native::resumes`]). Written by [`enter`]; read only by code with resume
    /// points (async functions, which are never called directly).
    pub resume: u64,
    pub chunk: *const Chunk,
    pub pad2: u64,
    pub env: *const Env,
    pub pad3: u64,
    pub this_val: *const Value,
    pub pad4: u64,
    /// Out-parameters of `Helper::TaView`: the viewed bytes' address and element count.
    pub ta_data: *mut u8,
    pub pad5: u64,
    pub ta_len: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::offset_of as o;
    assert!(o!(JitFrame, exception) % 16 == 0);
    assert!(o!(JitFrame, budget) % 16 == 0 && SAFEPOINT_BUDGET & 0xff == 0);
    assert!(o!(JitFrame, exit_depth) % 16 == 0);
    assert!(o!(JitFrame, exit_hset) % 16 == 0);
    assert!(o!(JitFrame, canon) % 16 == 0 && o!(JitFrame, resume) % 16 == 0);
    assert!(o!(JitFrame, pad2) % 16 == 0 && o!(JitFrame, pad3) % 16 == 0);
    assert!(o!(JitFrame, pad4) % 16 == 0 && o!(JitFrame, pad5) % 16 == 0);
    assert!(std::mem::size_of::<JitFrame>() == 160);
};

/// The lazy call record of a directly called frame (see `build::call` and [`sync_frames`]),
/// on the shadow stack right below the callee's frame. Both 16-byte-aligned words start with a
/// zero byte whatever they hold (see [`Shadow`]'s invariant), so the record needs no clearing
/// when the call returns.
#[repr(C)]
pub(crate) struct CallRec {
    /// `REC_*` bits (shifted left by 8: the low byte is always 0).
    pub flags: u32,
    /// The caller's call site (the record's `FnFrame::caller_site`).
    pub site: u32,
    /// The previous pending record (`Interp::jit_frames` before the call).
    pub link: usize,
    /// (32-bit hosts: keeps `pad` 16-byte aligned, as on 64-bit.)
    #[cfg(target_pointer_width = "32")]
    pub pad0: u32,
    /// Never written: its first byte (16-byte aligned) keeps whatever trivially droppable
    /// tag byte the shadow stack held.
    pub pad: u32,
    /// The caller's `Interp::cur_coro`.
    pub coro: u32,
    /// The callee's `Gc::as_ptr`.
    pub fn_ptr: usize,
    /// (32-bit hosts: keeps `pad2` 16-byte aligned.)
    #[cfg(target_pointer_width = "32")]
    pub pad1: u32,
    /// Never written (like `pad`).
    pub pad2: u32,
    #[cfg(target_pointer_width = "64")]
    pub pad3: u32,
    /// A callee with an activation layout: its owned activation environment for the call (the
    /// frame's `env` points here; dropped when the call returns). Unused otherwise.
    pub env: usize,
}

/// Shadow-stack bytes of a [`CallRec`].
pub(crate) const REC_BYTES: usize = 48;
const _: () = assert!(std::mem::size_of::<CallRec>() <= REC_BYTES);
pub(crate) const REC_FLAGS: i32 = std::mem::offset_of!(CallRec, flags) as i32;
pub(crate) const REC_LINK: i32 = std::mem::offset_of!(CallRec, link) as i32;
pub(crate) const REC_CORO: i32 = std::mem::offset_of!(CallRec, coro) as i32;
pub(crate) const REC_FN: i32 = std::mem::offset_of!(CallRec, fn_ptr) as i32;
pub(crate) const REC_ENV: i32 = std::mem::offset_of!(CallRec, env) as i32;
const _: () = assert!(std::mem::offset_of!(CallRec, pad2) == 32);
const _: () = assert!(REC_FLAGS == 0 && REC_CORO == 20 && std::mem::offset_of!(CallRec, site) == 4);

/// [`CallRec::flags`] bits (stored shifted left by 8).
pub(crate) const REC_STRICT: u32 = 1 << 8;
pub(crate) const REC_CONSTRUCT: u32 = 2 << 8;
/// The record was copied into `Interp::fn_frames` (the call pops it there when it returns).
pub(crate) const REC_SYNCED: u32 = 4 << 8;

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
pub(crate) const FRAME_CANON: i32 = std::mem::offset_of!(JitFrame, canon) as i32;
pub(crate) const FRAME_RESUME: i32 = std::mem::offset_of!(JitFrame, resume) as i32;
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
/// Calls per bytecode op a function makes before its first compile. Compiling costs tens of
/// microseconds per op (inlined callees included) while interpreting one costs tens of
/// nanoseconds: a large function, which runs only part of its ops per call, needs proportionally
/// more calls before the compile pays for itself. Functions of up to `1024 / FN_CALLS_PER_OP`
/// ops keep the plain `CALL_MASK + 1` threshold.
const FN_CALLS_PER_OP: u32 = 16;
/// Resume exits at one pc of function code before its speculation there is widened (the
/// function recompiles with the slots involved kept in memory).
const DEOPT_LIMIT: u32 = 32;

/// Function-tier states ([`ChunkJit::fstate`]).
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
    /// What compiles learned about sites whose receiver only a compile that sees it can
    /// resolve (a loop compiled while the locals hold it), by op pc, for later compiles (the
    /// whole-function tier compiles at entry, before the locals are set). Every use is guarded
    /// at run time, so a stale entry only costs a miss. See `build::call`.
    site_fb: RefCell<Vec<(usize, Box<dyn std::any::Any>)>>,
}

impl ChunkJit {
    /// The feedback of type `T` recorded for the site at `pc`.
    pub(crate) fn fb_get<T: Clone + 'static>(&self, pc: usize) -> Option<T> {
        let v = self.site_fb.borrow();
        v.iter()
            .find(|(q, _)| *q == pc)
            .and_then(|(_, b)| b.downcast_ref::<T>().cloned())
    }

    /// Record feedback `v` for the site at `pc` (replacing any).
    pub(crate) fn fb_put<T: 'static>(&self, pc: usize, v: T) {
        let mut f = self.site_fb.borrow_mut();
        f.retain(|(q, b)| *q != pc || !b.is::<T>());
        if f.len() < 256 {
            f.push((pc, Box::new(v)));
        }
    }

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

/// Shadow-stack bytes of a directly called frame: call record, header, slots, operand-stack
/// area.
pub(crate) fn frame_bytes(n_slots: usize, max_stack: usize) -> usize {
    REC_BYTES + FRAME_HDR + (n_slots + max_stack.max(1)) * VALUE_SIZE as usize
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

/// Shadow-stack bytes per thread. A call that would pass the end takes the generic (slow) call
/// path, so this bounds only how deep JIT-to-JIT calls stay direct. On Windows the zeroed
/// buffer is committed memory from the start (private bytes), so it is kept smaller there;
/// elsewhere untouched pages cost nothing.
#[cfg(all(target_pointer_width = "64", not(windows)))]
const SHADOW_BYTES: usize = 4 << 20;
#[cfg(all(target_pointer_width = "64", windows))]
const SHADOW_BYTES: usize = 1 << 20;
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

/// A direct call site's callee cache (see `build::call`): the callee last seen there — its
/// payload word, `fn_frames` identity and the address of the environment it closes over (inside
/// its callable record) — for the code's chunk `chunk`. `pin` keeps that callee alive so its
/// address cannot be reused by another object while cached. Rebound by `Helper::SiteRebind`
/// to any other function object with the same code (another closure of the same function).
#[repr(C)]
pub(crate) struct SiteCell {
    pub word: usize,
    pub fn_ptr: usize,
    pub env: usize,
    pub chunk: usize,
    pub pin: Value,
}

pub(crate) const CELL_WORD: i32 = std::mem::offset_of!(SiteCell, word) as i32;
pub(crate) const CELL_FN: i32 = std::mem::offset_of!(SiteCell, fn_ptr) as i32;
pub(crate) const CELL_ENV: i32 = std::mem::offset_of!(SiteCell, env) as i32;

/// The payload word of `v` as a `Value` stores it.
pub(crate) fn value_word(v: &Value) -> usize {
    // SAFETY: a `Value` is `VALUE_SIZE` bytes with its payload at `VALUE_PAYLOAD`.
    unsafe {
        *(v as *const Value as *const u8)
            .add(VALUE_PAYLOAD as usize)
            .cast::<usize>()
    }
}

/// The address of the environment user function `v` closes over (stable while `v` lives), and
/// its `Gc::as_ptr` identity.
pub(crate) fn callee_env_addr(v: &Value) -> Option<(usize, usize)> {
    let Value::Obj(o) = v else { return None };
    let b = o.try_borrow().ok()?;
    let crate::value::Callable::User(u) = &b.call else {
        return None;
    };
    Some((
        &u.env as *const Env as usize,
        crate::value::Gc::as_ptr(o) as usize,
    ))
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
    /// Resume points `(pc, operand-stack depth)` of function code: the op after each `await`,
    /// entered with `frame.resume` = 1 + the index (see [`on_resume`]).
    resumes: Vec<(usize, usize)>,
    /// See [`build::Built::num_exits`].
    num_exits: Vec<usize>,
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
    let t = stats_start();
    let r = build::build(i, env, chunk, header, backedge, slots, this_val)
        .and_then(|built| finish(built, header, backedge));
    stats_end(t, "loop", &r);
    r
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
    let t = stats_start();
    let r = build::build_fn(i, env, chunk, slots, widen, this_val)
        .and_then(|built| finish(built, 0, chunk.ops.len().saturating_sub(1)));
    stats_end(t, "fn", &r);
    r
}

/// `LUMEN_JIT_STATS`: one stderr line per compile attempt with its time and the running totals.
fn stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_JIT_STATS").is_some())
}

fn stats_start() -> Option<std::time::Instant> {
    stats_enabled().then(std::time::Instant::now)
}

fn stats_end(t: Option<std::time::Instant>, kind: &str, r: &Result<Native, String>) {
    let Some(t) = t else { return };
    thread_local! {
        static TOTAL: Cell<(u32, u32, u128)> = const { Cell::new((0, 0, 0)) };
    }
    let us = t.elapsed().as_micros();
    let (ok, err, sum) = TOTAL.get();
    let (ok, err) = if r.is_ok() { (ok + 1, err) } else { (ok, err + 1) };
    TOTAL.set((ok, err, sum + us));
    let what = match r {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("bail: {}", e.chars().take(60).collect::<String>()),
    };
    eprintln!(
        "[jit-stats] {kind} {us} us {what} | total {} us, {ok} ok, {err} bailed",
        sum + us
    );
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
        resumes: built.resumes,
        num_exits: built.num_exits,
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
        // A hot loop entered with operands pending (`s += a.map(x => …)`, see
        // `bytecode::inline_callback`) can't enter here: compile the whole function at its
        // next entry instead.
        if !stack.is_empty() && jit_enabled() && t & TICK_MASK == 0 && jit.fstate.get() == FS_COLD {
            jit.fstate.set(FS_PENDING);
        }
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
    enter(i, chunk, env, slots, stack, pc, this_val, Some((header, backedge)), &native, 0)
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
            // A cold large function waits until it has made enough calls (a pending one was
            // asked for by a hot loop, or retired and wants its recompile now).
            if jit.fstate.get() == FS_COLD
                && !eager()
                && jit.calls.get() / FN_CALLS_PER_OP < chunk.ops.len() as u32
            {
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
    enter(i, chunk, env, slots, stack, pc, this_val, None, &native, 0)
}

/// `LUMEN_JIT_RESUME`: give async function code resume points after its `await`s (see
/// [`on_resume`]). Off by default: an entry of native code costs more than the interpreter
/// spends on the short stretches between awaits of typical async code, and the settled values
/// arrive Boxed (a loop after an await runs better in the loop tier, which specializes on them).
pub(super) fn resume_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_JIT_RESUME").is_some())
}

/// Whether the driver of an async body continuing at `pc` (just after an `await`) may enter
/// its function code there (see [`on_resume`]).
#[inline(always)]
pub(super) fn resume_due(chunk: &Chunk, pc: usize) -> bool {
    chunk.jit.fstate.get() == FS_CODE
        && pc > 0
        && matches!(chunk.ops.get(pc - 1), Some(super::Op::Await))
}

/// Enter the function code of `chunk` at the resume point `*pc` after an `await`
/// (async-design.md §4.6.2), with the frame's operand stack (the settled value on top) moved
/// into the code's stack area. Returns like [`on_entry`]; `Ok(None)` also when the code has no
/// such resume point or its entry guards fail (the interpreter continues).
#[inline(never)]
pub(super) fn on_resume(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
) -> Result<Option<VmStep>, Abrupt> {
    let native = match chunk.jit.func.borrow().code.clone() {
        Some(n) => n,
        None => return Ok(None),
    };
    let Some(k) = native
        .resumes
        .iter()
        .position(|&(r, d)| r == *pc && d == stack.len())
    else {
        return Ok(None);
    };
    enter(i, chunk, env, slots, stack, pc, this_val, None, &native, k as u64 + 1)
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

/// The shortcut for a frame just entered (pc 0, empty stack, no handlers) of the JIT's call
/// helper and of prepared calls: run the chunk's function code without the interpreter's
/// driver. `None` when there is none, or when it exited part-way (the driver continues at
/// `*pc`).
#[inline(always)]
pub(super) fn run_entered(
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
/// [`JitFrame::canon`] of an activation the interpreter enters: 1 + `strict` when the engine
/// flags are exactly what a plain call of a function of that strictness sets up — `strict ==
/// tco_ok`, not `constructing`, no `new.target`, no field-initializer / async-generator-body
/// marker — else 0.
fn canon_flags(i: &Interp) -> u64 {
    let plain = i.strict == i.tco_ok
        && !i.constructing
        && matches!(i.new_target, Value::Undefined)
        && !i.in_field_init_code
        && !i.in_async_gen_body;
    if plain {
        1 + i.strict as u64
    } else {
        0
    }
}

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
    resume: u64,
) -> Result<Option<VmStep>, Abrupt> {
    let n = native.max_stack.max(1);
    // A resume entry takes over the frame's operand stack (its depth is the resume point's).
    let moved = if resume != 0 { stack.len() } else { 0 };
    if moved > n {
        return Ok(None);
    }
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
    for (k, v) in stack.drain(..).enumerate().take(moved) {
        area[k] = v;
    }
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
        canon: canon_flags(i),
        resume,
        pad2: 0,
        pad3: 0,
        pad4: 0,
        pad5: 0,
    };
    // SAFETY: the frame points at live state for the whole call; native code follows the
    // contract above (it touches only `slots`, `area` and the frame, and calls helpers).
    let word = unsafe { (native.entry)(&mut frame) };
    let kind = word & 0xff;
    let exit_pc = (word >> 8) as usize;
    let depth = (frame.exit_depth as usize).min(area.len());
    let exception = std::mem::take(&mut frame.exception);
    match kind {
        EXIT_ENTRY_FAIL if resume != 0 => {
            if log_enabled() {
                eprintln!("[jit] resume entry {resume} failed");
            }
            // The interpreter continues after the `await` (nothing to learn: the slots are
            // what the suspended body left).
            stack.extend(
                area[..moved]
                    .iter_mut()
                    .map(|v| std::mem::replace(v, Value::Undefined)),
            );
            Ok(None)
        }
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
                    if kind == EXIT_RESUME
                        && (native.guarded.contains(&exit_pc)
                            || helpers::take_ta_exit(chunk, exit_pc)
                            || (native.num_exits.contains(&exit_pc)
                                && helpers::note_num_exit(chunk, exit_pc - 1))) =>
                {
                    let mut loops = chunk.jit.loops.borrow_mut();
                    if let Some(l) = loops
                        .iter_mut()
                        .find(|l| l.header as usize == header && l.backedge as usize == backedge)
                    {
                        if log_enabled() {
                            eprintln!("[jit] loop {header}..={backedge} retired at {exit_pc}");
                        }
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
    // An `await` always exits (see `build`): nothing to learn from it.
    if matches!(chunk.ops.get(pc), Some(super::Op::Await)) {
        return;
    }
    let jit = &chunk.jit;
    let mut fs = jit.func.borrow_mut();
    let guarded = native.guarded.contains(&pc)
        || (native.num_exits.contains(&pc) && helpers::note_num_exit(chunk, pc - 1));
    if (guarded || helpers::take_ta_exit(chunk, pc))
        && fs.code.as_ref().is_some_and(|c| std::ptr::eq(&**c, native))
    {
        // A guarded call site stopped holding (or a typed-array site wants a view cache):
        // recompile.
        if log_enabled() {
            let why = if guarded { "guard failed" } else { "typed-array site hot" };
            eprintln!("[jit] function retired at {pc}: {why}");
        }
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
                // An op may drop every handler before throwing (`DerivedReturn`: [[Construct]]'s
                // own TypeError is not catchable by the body); it then propagates.
                let Some(h) = hs.pop() else {
                    return Err(Abrupt::Throw(e));
                };
                stack.truncate(h.stack_depth);
                stack.push(e);
                *pc = h.catch_pc;
            }
            Err(other) => return Err(other),
        }
    }
    Ok(None)
}

// ---- lazy call records of directly called frames ------------------------------------------------
//
// A call native code makes directly (see `build::call`) does not push an `Interp::fn_frames`
// entry. It fills the record fields of its callee's `JitFrame` header and links it from
// `Interp::jit_frames` (newest first); the call pops it by restoring the link. Pending records
// are always logically *above* every `fn_frames` entry: anything that pushes to `fn_frames`
// first calls [`sync_frames`], which copies the pending records in (oldest first) and marks
// them `REC_SYNCED` (such a call pops its `fn_frames` entry when it returns). The full call
// stack, oldest first, is therefore `fn_frames` followed by [`pending_frames`]. A stack trace
// reads it that way; code that needs `&mut` access to a frame calls `sync_frames` first.

/// Parameter values for a directly entered frame (see [`call_direct`]): at most `cap` are
/// kept, surplus ones dropped.
pub(crate) struct Seed {
    p: *mut Value,
    n: usize,
    cap: usize,
}

impl Seed {
    #[inline(always)]
    pub(crate) fn push(&mut self, v: Value) {
        if self.n < self.cap {
            // SAFETY: `p[..cap]` are the frame's (uninitialized) parameter slots.
            unsafe { self.p.add(self.n).write(v) };
            self.n += 1;
        } else {
            super::drop_value_fast(v);
        }
    }

    /// Whether another parameter would be kept.
    #[inline(always)]
    pub(crate) fn wants(&self) -> bool {
        self.n < self.cap
    }
}

/// Run `seed` into `p` (room for `chunk`'s parameters): the number of values written.
///
/// # Safety
/// `p` has room for `min(n_params, n_slots)` values.
pub(crate) unsafe fn seed_raw(p: *mut Value, chunk: &Chunk, seed: impl FnOnce(&mut Seed)) -> usize {
    let mut s = Seed {
        p,
        n: 0,
        cap: chunk.n_params.min(chunk.n_slots),
    };
    seed(&mut s);
    s.n
}

/// Whether [`call_direct`] can run `chunk` now: it has function code and the shadow stack has
/// room for its frame.
#[inline(always)]
pub(crate) fn direct_ready(chunk: &Chunk) -> bool {
    let entry = chunk.jit.dentry.get();
    // SAFETY: the shadow stack is this thread's and outlives it.
    entry != 0 && unsafe {
        let sh = shadow();
        (*sh).top + chunk.jit.dsize.get() <= (*sh).end
    }
}

/// A call of `chunk`'s function code straight from Rust (a prepared callback, see
/// `bytecode::prepared_call`), made as a direct call site makes it (see `build::call`): the
/// frame on the shadow stack, the code entered at its direct entry, a lazy call record for
/// `fn_frames`, and any exit other than a return finished by the interpreter on the same slots
/// (`helpers::finish_exit`). The caller checked [`direct_ready`] and did `Interp::call`'s
/// entry work (the recursion depth, the GC poll) and does its exit work (a pending proper tail
/// call, the depth). `seed` writes the arguments.
///
/// # Safety
/// `chunk`, `env`, `fn_ptr` and the flags are those of a live compiled function (see
/// `prepared_call::Compiled`).
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn call_direct(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    fn_ptr: usize,
    strict: bool,
    arrow: bool,
    this: &Value,
    seed: impl FnOnce(&mut Seed),
) -> Result<Value, Abrupt> {
    let entry = chunk.jit.dentry.get();
    let sh = shadow();
    let top0 = (*sh).top;
    (*sh).top = top0 + chunk.jit.dsize.get();
    // --- enter_frame (an ordinary call) ---
    let saved_ctor = std::mem::replace(&mut i.constructing, false);
    let saved_nt = if !arrow && !matches!(i.new_target, Value::Undefined) {
        Some(std::mem::replace(&mut i.new_target, Value::Undefined))
    } else {
        None
    };
    let this_val = if chunk.uses_this() {
        i.bind_compiled_this_flags(strict, chunk, this.clone(), false)
    } else {
        Value::Undefined
    };
    let saved_strict = std::mem::replace(&mut i.strict, strict);
    let saved_tco = std::mem::replace(&mut i.tco_ok, strict);
    let saved_field_init = i.in_field_init_code;
    let saved_agb = i.in_async_gen_body;
    if !arrow {
        i.in_field_init_code = false;
        i.in_async_gen_body = false;
    }
    // [record | header | slots | operand-stack area] (the area needs no clearing, see `Shadow`)
    let rec = top0 as *mut CallRec;
    let nf = (top0 + REC_BYTES) as *mut JitFrame;
    let slots = (top0 + REC_BYTES + FRAME_HDR) as *mut Value;
    let n = chunk.n_slots;
    let mut s = Seed {
        p: slots,
        n: 0,
        cap: chunk.n_params.min(n),
    };
    seed(&mut s);
    for k in s.n..n {
        slots.add(k).write(Value::Undefined);
    }
    for &k in &chunk.var_force_resets {
        super::drop_value_fast(std::ptr::replace(slots.add(k as usize), Value::Undefined));
    }
    nf.write(JitFrame {
        exception: Value::Undefined,
        budget: SAFEPOINT_BUDGET,
        slots,
        exit_depth: 0,
        consts: chunk.consts.as_ptr(),
        exit_hset: 0,
        stack: slots.add(n),
        canon: canon_flags(i),
        interp: i as *mut Interp,
        resume: 0,
        chunk: chunk as *const Chunk,
        pad2: 0,
        env: env as *const Env,
        pad3: 0,
        this_val: &this_val as *const Value,
        pad4: 0,
        ta_data: std::ptr::null_mut(),
        pad5: 0,
        ta_len: 0,
    });
    // (The record's `pad` keeps its byte, see `CallRec`.)
    (*rec).flags = if strict { REC_STRICT } else { 0 };
    (*rec).site = std::mem::replace(&mut i.cur_site, crate::interpreter::frames::NO_SITE);
    (*rec).link = i.jit_frames;
    (*rec).coro = i.cur_coro;
    (*rec).fn_ptr = fn_ptr;
    i.jit_frames = rec as usize;
    // --- run ---
    let code: NativeFn = std::mem::transmute::<usize, NativeFn>(entry);
    let word = code(nf);
    let r = if word & 0xff == EXIT_RETURN {
        let v = std::ptr::replace((*nf).stack, Value::Undefined);
        for k in 0..n {
            super::drop_value_fast(std::ptr::replace(slots.add(k), Value::Undefined));
        }
        Ok(v)
    } else {
        helpers::finish_exit(i, nf, word, entry)
    };
    // --- leave_frame ---
    if (*rec).flags & REC_SYNCED != 0 {
        i.fn_frames.pop();
    }
    i.jit_frames = (*rec).link;
    i.cur_site = (*rec).site;
    if cfg!(target_pointer_width = "32") {
        let mut off = 0;
        while off < FRAME_HDR {
            *(nf as *mut u8).add(off) = 0;
            off += 16;
        }
    }
    (*sh).top = top0;
    super::drop_value_fast(this_val);
    i.strict = saved_strict;
    i.tco_ok = saved_tco;
    i.in_field_init_code = saved_field_init;
    i.in_async_gen_body = saved_agb;
    i.constructing = saved_ctor;
    if let Some(nt) = saved_nt {
        super::drop_value_fast(std::mem::replace(&mut i.new_target, nt));
    }
    r
}

/// Copy the pending direct-call records into `i.fn_frames` (see above). Call before pushing to
/// `fn_frames`, and before reading it with `&mut` access (`f.caller` / `f.arguments`).
#[inline(always)]
pub(crate) fn sync_frames(i: &mut Interp) {
    if i.jit_frames != 0 {
        sync_frames_slow(i);
    }
}

/// The pending records, newest first (stops at the first synced one: every record below it
/// was synced too).
fn pending_recs(i: &Interp) -> Vec<*mut CallRec> {
    let mut v = Vec::new();
    let mut p = i.jit_frames as *mut CallRec;
    // SAFETY: every linked record belongs to a live directly called frame (a call unlinks its
    // record before its frame is released).
    unsafe {
        while !p.is_null() && (*p).flags & REC_SYNCED == 0 {
            v.push(p);
            p = (*p).link as *mut CallRec;
        }
    }
    v
}

fn rec_frame(p: *const CallRec) -> crate::interpreter::FnFrame {
    // SAFETY: see `pending_recs`.
    let r = unsafe { &*p };
    crate::interpreter::FnFrame {
        fn_ptr: r.fn_ptr,
        coro: r.coro,
        caller_site: r.site,
        strict: r.flags & REC_STRICT != 0,
        construct: r.flags & REC_CONSTRUCT != 0,
        extra: None,
    }
}

#[cold]
#[inline(never)]
fn sync_frames_slow(i: &mut Interp) {
    let recs = pending_recs(i);
    for &p in recs.iter().rev() {
        i.fn_frames.push(rec_frame(p));
        // SAFETY: see `pending_recs`.
        unsafe { (*p).flags |= REC_SYNCED };
    }
}

/// The pending direct-call records as frames, oldest first: they go on top of `i.fn_frames`
/// (see above). Empty — and allocation-free — when there are none.
pub(crate) fn pending_frames(i: &Interp) -> Vec<crate::interpreter::FnFrame> {
    if i.jit_frames == 0 {
        return Vec::new();
    }
    pending_recs(i).into_iter().rev().map(|p| rec_frame(p)).collect()
}

/// A coroutine body died (its worker is gone): unlink its pending records, like the
/// `fn_frames` entries it owned.
pub(crate) fn evict_coro_frames(i: &mut Interp, coro: u32) {
    let mut keep = Vec::new();
    let mut p = i.jit_frames as *mut CallRec;
    // SAFETY: see `pending_recs` (a dead body's frames are parked, not released).
    unsafe {
        while !p.is_null() {
            if (*p).coro != coro {
                keep.push(p);
            }
            p = (*p).link as *mut CallRec;
        }
        for w in keep.windows(2) {
            (*w[0]).link = w[1] as usize;
        }
        if let Some(&last) = keep.last() {
            (*last).link = 0;
        }
    }
    i.jit_frames = keep.first().map_or(0, |&p| p as usize);
}
