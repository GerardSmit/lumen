//! Byte estimates of a compiled [`Chunk`] for the `LUMEN_MEM_STATS` report (see
//! [`crate::memstats`]). Diagnostics only.

use std::mem::size_of;
use std::rc::Rc;

use super::Chunk;
use crate::ast::Function;

/// A chunk's heap bytes by part.
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct ChunkBytes {
    /// The `Chunk` struct itself (its `Rc` box).
    pub header: usize,
    /// The op vector.
    pub ops: usize,
    /// Constant pool, name tables, inner-function lists, captured-binding plans.
    pub pools: usize,
    /// Inline caches and their side vectors (property, name, captured-binding, literal maps,
    /// switch tables).
    pub ics: usize,
    /// Call-site position table.
    pub positions: usize,
    /// Capacity beyond length across the vectors above.
    pub slack: usize,
}

impl ChunkBytes {
    pub(crate) fn add(&mut self, o: &ChunkBytes) {
        self.header += o.header;
        self.ops += o.ops;
        self.pools += o.pools;
        self.ics += o.ics;
        self.positions += o.positions;
        self.slack += o.slack;
    }

    pub(crate) fn total(&self) -> usize {
        self.header + self.ops + self.pools + self.ics + self.positions
    }
}

fn vec_bytes<T>(v: &Vec<T>, slack: &mut usize) -> usize {
    *slack += (v.capacity() - v.len()) * size_of::<T>();
    v.capacity() * size_of::<T>()
}

impl Chunk {
    /// Estimated heap bytes (see [`ChunkBytes`]).
    pub(crate) fn mem_bytes(&self) -> ChunkBytes {
        let mut b = ChunkBytes {
            header: 2 * size_of::<usize>() + size_of::<Chunk>(),
            ..Default::default()
        };
        let s = &mut b.slack;
        b.ops = vec_bytes(&self.ops, s);
        b.pools = vec_bytes(&self.consts, s)
            + vec_bytes(&self.names, s)
            + vec_bytes(&self.slot_names, s)
            + vec_bytes(&self.var_force_resets, s)
            + vec_bytes(&self.funcs, s)
            + vec_bytes(&self.classes, s)
            + vec_bytes(&self.cap_inits, s);
        b.ics = vec_bytes(&self.caches, s)
            + vec_bytes(&self.obj_maps, s)
            + vec_bytes(&self.name_caches, s)
            + vec_bytes(&self.name_paths, s)
            + vec_bytes(&self.name_pins.borrow(), s)
            + vec_bytes(&self.cap_caches, s)
            + vec_bytes(&self.cap_pins.borrow(), s)
            + vec_bytes(&self.switch_tables, s);
        b.positions = self.positions.len();
        b
    }

    /// The function nodes this chunk instantiates (`MakeClosure` templates).
    pub(crate) fn inner_functions(&self) -> &[Rc<Function>] {
        &self.funcs
    }
}
