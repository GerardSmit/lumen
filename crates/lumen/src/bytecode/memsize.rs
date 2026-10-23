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
        parts[0] = self.caches.heap_bytes();
        parts[1] = vec_bytes(&self.obj_maps, s);
        parts[2] = vec_bytes(&self.name_caches, s) + vec_bytes(&self.name_chains, s);
        parts[3] = vec_bytes(&self.name_paths, s);
        parts[4] = vec_bytes(&self.name_pins.borrow(), s);
        parts[5] = vec_bytes(&self.cap_caches, s);
        parts[6] = vec_bytes(&self.cap_pins.borrow(), s);
        parts[7] = vec_bytes(&self.switch_tables, s);
        b.ic_sites = self.caches.len();
        let states: Vec<super::IcState> = (0..self.caches.len())
            .map(|i| self.caches.peek(i).unwrap_or(super::IcState::EMPTY))
            .collect();
        for ic in &states {
            if ic.depth != super::IC_EMPTY {
                b.ic_filled += 1;
                if ic.mid_ok != 0 {
                    b.ic_deep += 1;
                }
            }
        }
        for (k, size) in [8usize, 16, 32].into_iter().enumerate() {
            for page in states.chunks(size) {
                b.ic_page_total[k] += 1;
                if page.iter().any(|ic| ic.depth != super::IC_EMPTY) {
                    b.ic_pages[k] += 1;
                }
            }
        }
        if self.caches.len() != 0 && b.ic_filled == 0 {
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

#[cfg(test)]
mod tests {
    use super::super::{compile, PropCaches, IC_EMPTY, PROP_IC_WAYS};
    use std::mem::size_of;

    fn chunk_of(src: &str) -> std::rc::Rc<super::super::Chunk> {
        let stmts = crate::parser::parse_script(src, false).ok().expect("parse");
        let func = stmts
            .iter()
            .find_map(|s| match s {
                crate::ast::Stmt::FuncDecl(f) => Some(f.clone()),
                _ => None,
            })
            .expect("function declaration");
        compile(&func).expect("compiles")
    }

    #[test]
    fn unrun_chunk_holds_no_property_cache_pages() {
        let chunk = chunk_of("function f(o){ o.a = o.b + o.c; return o.d instanceof Object; }");
        let sites = chunk.caches.len() / PROP_IC_WAYS;
        assert!(sites >= 4, "expected several cache sites, got {sites}");
        assert_eq!(chunk.caches.allocated_pages().count(), 0);
        let table = chunk.caches.heap_bytes();
        assert_eq!(table, sites * size_of::<usize>());
        assert_eq!(chunk.mem_bytes().ic_parts[0], table);
        assert!(chunk.caches.peek(0).is_some_and(|st| st.depth == IC_EMPTY));
        assert!(chunk.caches.get_allocated(0).is_none());
        assert_eq!(chunk.caches.allocated_pages().count(), 0);
    }

    #[test]
    fn first_access_allocates_only_its_site_page() {
        let caches = PropCaches::new(3 * PROP_IC_WAYS);
        assert_eq!(caches.len(), 3 * PROP_IC_WAYS);
        let way = caches.at(PROP_IC_WAYS + 1);
        assert_eq!(caches.allocated_pages().count(), 1);
        assert!(caches.get_allocated(0).is_none());
        assert!(!std::ptr::eq(way, caches.at(PROP_IC_WAYS)));
        let before = way as *const _;
        let _ = caches.at(2 * PROP_IC_WAYS);
        assert_eq!(caches.allocated_pages().count(), 2);
        assert!(std::ptr::eq(before, caches.at(PROP_IC_WAYS + 1)));
        assert!(caches.get(3 * PROP_IC_WAYS).is_none());
        assert!(caches.peek(3 * PROP_IC_WAYS).is_none());
    }
}
