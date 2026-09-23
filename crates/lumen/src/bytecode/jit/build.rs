//! Bytecode region → lumen-codegen IR. See the contract in [`super`].
//!
//! The translator walks the region's ops in pc order, one IR block per bytecode basic block,
//! with an abstract operand stack of typed entries ([`Kind`]) and the region's `Num` / `Bool`
//! locals as IR variables (SSA via the builder's `def_var` / `use_var`). Unboxed arithmetic and
//! comparisons are emitted inline; everything else goes through a speculation guard (exit on
//! failure), a [`super::layout`] fast path, or a [`super::helpers`] call.

use super::*;

/// A translated region.
pub(crate) struct Built {
    /// `(I64 frame) -> I64 exit word`.
    pub func: lumen_codegen::Function,
    /// Entries `frame.stack` must provide.
    pub max_stack: usize,
    /// The representation chosen for each slot (`chunk.n_slots` long); `Num`/`Bool` slots are
    /// guarded at entry.
    pub kinds: Vec<Kind>,
}

/// Translate the loop `[header, backedge]` of `chunk`, specializing on the current `slots`.
/// `Err` (with a reason for `LUMEN_TIER_LOG`) when the region uses something unsupported.
pub(crate) fn build(chunk: &Chunk, header: usize, backedge: usize, slots: &[Value]) -> Result<Built, String> {
    let _ = (chunk, header, backedge, slots);
    todo!("translator agent")
}
