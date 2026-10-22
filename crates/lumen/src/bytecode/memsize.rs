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
    /// `ics` by part: property caches, object-literal maps, name caches, name paths, name pins,
    /// captured caches, captured pins, switch tables.
    pub ic_parts: [usize; 8],
    /// Property cache sites and how many were ever filled / filled with a depth >= 2 chain.
    pub ic_sites: usize,
    pub ic_filled: usize,
    pub ic_deep: usize,
    /// Chunks with property caches none of which was ever filled, and the cache bytes they hold.
    pub ic_idle_chunks: usize,
    pub ic_idle_bytes: usize,
    /// Property cache pages (of 8, 16, 32 sites) holding a filled site, per page size.
    pub ic_pages: [usize; 3],
    pub ic_page_total: [usize; 3],
}

impl ChunkBytes {
    pub(crate) fn add(&mut self, o: &ChunkBytes) {
        self.header += o.header;
        self.ops += o.ops;
        self.pools += o.pools;
        self.ics += o.ics;
        self.positions += o.positions;
        self.slack += o.slack;
        self.ic_sites += o.ic_sites;
        self.ic_filled += o.ic_filled;
        self.ic_deep += o.ic_deep;
        self.ic_idle_chunks += o.ic_idle_chunks;
        self.ic_idle_bytes += o.ic_idle_bytes;
        for k in 0..3 {
            self.ic_pages[k] += o.ic_pages[k];
            self.ic_page_total[k] += o.ic_page_total[k];
        }
        for (a, b) in self.ic_parts.iter_mut().zip(o.ic_parts) {
            *a += b;
        }
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
        let mut parts = [0usize; 8];
        parts[0] = vec_bytes(&self.caches, s);
        parts[1] = vec_bytes(&self.obj_maps, s);
        parts[2] = vec_bytes(&self.name_caches, s);
        parts[3] = vec_bytes(&self.name_paths, s);
        parts[4] = vec_bytes(&self.name_pins.borrow(), s);
        parts[5] = vec_bytes(&self.cap_caches, s);
        parts[6] = vec_bytes(&self.cap_pins.borrow(), s);
        parts[7] = vec_bytes(&self.switch_tables, s);
        b.ic_sites = self.caches.len();
        for c in &self.caches {
            let ic = c.get();
            if ic.depth != super::IC_EMPTY {
                b.ic_filled += 1;
                if ic.mid_ok != 0 {
                    b.ic_deep += 1;
                }
            }
        }
        for (k, size) in [8usize, 16, 32].into_iter().enumerate() {
            for page in self.caches.chunks(size) {
                b.ic_page_total[k] += 1;
                if page.iter().any(|c| c.get().depth != super::IC_EMPTY) {
                    b.ic_pages[k] += 1;
                }
            }
        }
        if !self.caches.is_empty() && b.ic_filled == 0 {
            b.ic_idle_chunks = 1;
            b.ic_idle_bytes = parts[0];
        }
        b.ics = parts.iter().sum();
        b.ic_parts = parts;
        b.positions = self.positions.len();
        b
    }

    /// The function nodes this chunk instantiates (`MakeClosure` templates).
    pub(crate) fn inner_functions(&self) -> &[Rc<Function>] {
        &self.funcs
    }
}
