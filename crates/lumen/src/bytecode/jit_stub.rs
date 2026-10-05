//! Bytecode runtime hooks when the optimizing JIT is not linked.

use super::{Chunk, VmStep};
use crate::interpreter::{Abrupt, Env, FnFrame, Interp};
use crate::value::Value;
use std::rc::Rc;

pub(crate) use super::seed::{seed_raw, Seed};

#[derive(Default)]
pub(crate) struct ChunkJit;

/// The VM keeps this field layout independent of whether native frames exist.
pub(crate) struct Shadow;

pub(super) fn track_chunk(chunk: Rc<Chunk>) -> Rc<Chunk> {
    chunk
}

pub(crate) fn prune_dead_chunks() {}

pub unsafe fn evict_quiescent_code() -> usize {
    0
}

#[inline(always)]
pub(super) fn backedge_due(_: &Interp, _: &Chunk) -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn on_backedge(
    _: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: &mut [Value],
    _: &mut Vec<Value>,
    _: &mut usize,
    _: &Value,
    _: usize,
) -> Result<Option<VmStep>, Abrupt> {
    Ok(None)
}

#[inline(always)]
pub(super) fn entry_due(_: &Interp, _: &Chunk) -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn on_entry(
    _: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: &mut [Value],
    _: &mut Vec<Value>,
    _: &mut usize,
    _: &Value,
) -> Result<Option<VmStep>, Abrupt> {
    Ok(None)
}

#[inline(always)]
pub(super) fn resume_due(_: &Interp, _: &Chunk, _: usize) -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn on_resume(
    _: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: &mut [Value],
    _: &mut Vec<Value>,
    _: &mut usize,
    _: &Value,
) -> Result<Option<VmStep>, Abrupt> {
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_entered(
    _: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: &mut [Value],
    _: &mut Vec<Value>,
    _: &mut usize,
    _: &Value,
) -> Option<Result<VmStep, Abrupt>> {
    None
}

#[inline(always)]
pub(crate) fn direct_ready(_: &mut Interp, _: &Chunk) -> bool {
    false
}

/// Only called after `direct_ready`; a disabled JIT never enters this path.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn call_direct(
    _: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: usize,
    _: bool,
    _: bool,
    _: &Value,
    _: impl FnOnce(&mut Seed),
) -> Result<Value, Abrupt> {
    unreachable!("direct native call with JIT disabled")
}

#[inline(always)]
pub(crate) fn sync_frames(_: &mut Interp) {}

pub(crate) fn pending_frames(_: &Interp) -> Vec<FnFrame> {
    Vec::new()
}

pub(crate) fn evict_coro_frames(_: &mut Interp, _: u32) {}

#[derive(Default)]
pub(crate) struct EngineJit;

#[inline(always)]
pub(crate) fn trim_check(_: &Interp) {}
