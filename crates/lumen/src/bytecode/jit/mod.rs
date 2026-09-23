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
//! `(I64) -> I64`. No traps, no stack-limit check: every failure is a returned exit word.
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
}

pub(crate) const FRAME_SLOTS: i32 = 0;
pub(crate) const FRAME_CONSTS: i32 = 8;
pub(crate) const FRAME_STACK: i32 = 16;
pub(crate) const FRAME_INTERP: i32 = 24;
pub(crate) const FRAME_CHUNK: i32 = 32;
pub(crate) const FRAME_ENV: i32 = 40;
pub(crate) const FRAME_THIS: i32 = 48;
pub(crate) const FRAME_EXIT_DEPTH: i32 = 56;
pub(crate) const FRAME_EXCEPTION: i32 = 64;

const _: () = {
    assert!(std::mem::offset_of!(JitFrame, slots) == FRAME_SLOTS as usize);
    assert!(std::mem::offset_of!(JitFrame, consts) == FRAME_CONSTS as usize);
    assert!(std::mem::offset_of!(JitFrame, stack) == FRAME_STACK as usize);
    assert!(std::mem::offset_of!(JitFrame, interp) == FRAME_INTERP as usize);
    assert!(std::mem::offset_of!(JitFrame, chunk) == FRAME_CHUNK as usize);
    assert!(std::mem::offset_of!(JitFrame, env) == FRAME_ENV as usize);
    assert!(std::mem::offset_of!(JitFrame, this_val) == FRAME_THIS as usize);
    assert!(std::mem::offset_of!(JitFrame, exit_depth) == FRAME_EXIT_DEPTH as usize);
    assert!(std::mem::offset_of!(JitFrame, exception) == FRAME_EXCEPTION as usize);
    assert!(std::mem::size_of::<Value>() == VALUE_SIZE as usize);
};

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
