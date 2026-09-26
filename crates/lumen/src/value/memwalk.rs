//! The structural half of the `LUMEN_MEM_STATS` report (see [`crate::memstats`]): walks this
//! thread's heap state — the object slab, property storage, scopes, shapes and the registered
//! function nodes — and estimates the bytes behind each. Diagnostics only; never on a hot path.

use std::mem::size_of;
use std::rc::Rc;

use super::{with_gc_state, Callable, Property};

/// Structural byte estimates for one heap.
#[derive(Default, Debug)]
pub(crate) struct HeapWalk {
    /// Per slot class: `(chunks, used slots, slots ever handed out, slot size)`.
    pub slab: [(usize, usize, usize, usize); 4],
    pub objects: usize,
    pub user_fns: usize,
    /// Owned property entries (len, capacity) — shared template blocks excluded.
    pub prop_len: usize,
    pub prop_cap: usize,
    pub scopes: usize,
    pub scope_bytes: usize,
    pub shapes: usize,
    pub shape_bytes: usize,
    /// Distinct function nodes (from closures and the lazy registry).
    pub fn_nodes: usize,
    pub fn_node_bytes: usize,
    /// Of those: nodes whose body is currently materialised, and their top-level statements.
    pub bodies: usize,
    pub body_stmts: usize,
    /// Nodes with a compiled chunk, and the chunks' estimated bytes by part.
    pub chunks: usize,
    pub chunk: crate::bytecode::ChunkBytes,
    pub lazy_registry: usize,
    /// Parameters of the walked function nodes (count, and their vectors' capacity).
    pub params: usize,
    pub params_cap: usize,
    /// Lazy `split` results (see [`crate::split_view`]): count, offset-table bytes, and the
    /// bytes of their distinct source strings.
    pub split_views: usize,
    pub split_view_offsets: usize,
    pub split_view_src: usize,
}

pub(crate) fn walk() -> HeapWalk {
    let mut w = HeapWalk {
        slab: with_gc_state(|s| s.heap.census()),
        ..Default::default()
    };
    let objs = super::gc_snapshot();
    w.objects = objs.len();
    let mut nodes: std::collections::HashSet<*const crate::ast::Function> = Default::default();
    let mut fns: Vec<Rc<crate::ast::Function>> = Vec::new();
    let mut view_srcs: std::collections::HashSet<*const u8> = Default::default();
    for o in &objs {
        let b = o.borrow();
        if let Callable::SplitView(v) = &b.call {
            w.split_views += 1;
            w.split_view_offsets += v.offsets_bytes();
            let s = v.source().as_str();
            if view_srcs.insert(s.as_ptr()) {
                w.split_view_src += s.len();
            }
        }
        let c = b.props.census();
        if !c.entries_shared {
            w.prop_len += c.entries_len;
            w.prop_cap += c.entries_cap;
        }
        if let Callable::User(u) = &b.call {
            w.user_fns += 1;
            if nodes.insert(Rc::as_ptr(&u.func)) {
                fns.push(u.func.clone());
            }
        }
    }
    drop(objs);
    with_gc_state(|s| {
        let reg = s.lazy_fns.borrow();
        w.lazy_registry = reg.len();
        for f in reg.iter().filter_map(|f| f.upgrade()) {
            if nodes.insert(Rc::as_ptr(&f)) {
                fns.push(f);
            }
        }
    });
    // Chunks reference inner function nodes that may not have a closure yet.
    let mut k = 0;
    while k < fns.len() {
        let f = fns[k].clone();
        k += 1;
        if let Some(Some(chunk)) = f.code.get() {
            for inner in chunk.inner_functions() {
                if nodes.insert(Rc::as_ptr(inner)) {
                    fns.push(inner.clone());
                }
            }
        }
    }
    let rc_box = 2 * size_of::<usize>();
    for f in &fns {
        w.fn_nodes += 1;
        w.fn_node_bytes += rc_box + size_of::<crate::ast::Function>();
        w.fn_node_bytes += f.params.capacity() * size_of::<crate::ast::Param>();
        w.params += f.params.len();
        w.params_cap += f.params.capacity();
        w.fn_node_bytes += f.name.as_ref().map_or(0, |n| n.capacity());
        if f.lazy.borrow().is_some() {
            w.fn_node_bytes += size_of::<crate::ast::LazyBody>();
        }
        if let Some(body) = f.parsed_body() {
            w.bodies += 1;
            w.body_stmts += body.len();
        }
        if let Some(Some(chunk)) = f.code.get() {
            w.chunks += 1;
            w.chunk.add(&chunk.mem_bytes());
        }
    }
    let scopes = super::scope_snapshot();
    w.scopes = scopes.len();
    for e in &scopes {
        let (_, cap, _) = e.borrow().vars.census();
        w.scope_bytes += rc_box
            + size_of::<std::cell::RefCell<crate::interpreter::Scope>>()
            + cap * size_of::<(Rc<str>, crate::interpreter::Binding)>();
    }
    let sc = super::shape_table_census();
    w.shapes = sc.shapes;
    w.shape_bytes = sc.bytes;
    let _ = size_of::<Property>();
    w
}

/// Bytes of one property entry.
pub(crate) fn property_size() -> usize {
    size_of::<Property>()
}
