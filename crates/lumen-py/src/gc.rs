//! The cycle collector: CPython's generational garbage collector over `Rc` objects.
//!
//! Reference counting frees everything that is not part of a cycle, immediately. Container objects
//! are additionally kept in three generation lists (plus a permanent one for `gc.freeze`); a
//! collection of generation `g` takes the objects of generations `0..=g`, subtracts the references
//! they hold on each other from their reference counts (the shared core in
//! [`lumen_common::cycle`]), and what is left unreferenced from outside is cyclic garbage. As in
//! CPython 3.12 (PEP 442) the garbage then gets its weak references cleared and their callbacks
//! called, its `__del__` / generator finalizers run once, a recheck for resurrection, and finally
//! `tp_clear` ([`crate::gc_traverse::clear`]) on what is still unreachable, which lets the
//! reference counts free it.
//!
//! The lists hold raw pointers, kept in step by `Object::alloc` / `Drop for Object`: an object
//! knows its slot (`GcCell::idx`) so leaving a list is a `swap_remove`. No list operation ever
//! drops an object or runs script code, so the heap cell is never borrowed re-entrantly.
//!
//! `__del__` and generator close run on refcount death too: `Drop` has no interpreter, so a dying
//! object that needs finalizing is moved into a fresh allocation (keeping its identity and weak
//! references) and queued; the poll sites of the VM run the queue.

use crate::gc_traverse;
use crate::object::*;
use crate::vm::*;
use lumen_common::cycle::{self, GenStats, Generations, Graph, GENERATIONS};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub const UNTRACKED: u8 = 255;
const PERMANENT: usize = 3;
/// Set while the object is in the unreachable set of a running collection.
const GC_UNREACH: u8 = 2;

pub const DEBUG_STATS: u32 = 1;
pub const DEBUG_COLLECTABLE: u32 = 2;
pub const DEBUG_UNCOLLECTABLE: u32 = 4;
pub const DEBUG_SAVEALL: u32 = 32;

struct Heap {
    lists: [Vec<*const Object>; 4],
    sched: Generations,
    enabled: bool,
    collecting: bool,
    seq: u32,
}

impl Heap {
    const fn new() -> Heap {
        Heap { lists: [Vec::new(), Vec::new(), Vec::new(), Vec::new()], sched: Generations::new(), enabled: true, collecting: false, seq: 0 }
    }
}

thread_local! {
    static HEAP: RefCell<Heap> = const { RefCell::new(Heap::new()) };
    static DUE: Cell<bool> = const { Cell::new(false) };
    static EPOCH: Cell<u64> = const { Cell::new(0) };
    static FINALIZING: Cell<bool> = const { Cell::new(false) };
    static FINALIZERS: RefCell<Vec<Obj>> = const { RefCell::new(Vec::new()) };
}

/// The collector's bookkeeping, which follows the interpreter from thread to thread as the GIL
/// changes hands (see `threads`): the generation lists hold raw pointers to every container, so
/// they must be one set, not one per OS thread.
pub(crate) struct HeapState {
    heap: Heap,
    due: bool,
    epoch: u64,
    finalizing: bool,
    finalizers: Vec<Obj>,
}

pub(crate) fn state_take() -> HeapState {
    HeapState {
        heap: HEAP.with(|h| std::mem::replace(&mut *h.borrow_mut(), Heap::new())),
        due: DUE.with(|d| d.replace(false)),
        epoch: EPOCH.with(|e| e.replace(0)),
        finalizing: FINALIZING.with(|f| f.replace(false)),
        finalizers: FINALIZERS.with(|f| std::mem::take(&mut *f.borrow_mut())),
    }
}

pub(crate) fn state_put(state: HeapState) {
    let old = HEAP.with(|h| std::mem::replace(&mut *h.borrow_mut(), state.heap));
    DUE.with(|d| d.set(state.due));
    EPOCH.with(|e| e.set(state.epoch));
    FINALIZING.with(|f| f.set(state.finalizing));
    let finalizers = FINALIZERS.with(|f| std::mem::replace(&mut *f.borrow_mut(), state.finalizers));
    drop(old);
    drop(finalizers);
}

fn remove_at(list: &mut Vec<*const Object>, idx: usize) {
    let i = idx - 1;
    list.swap_remove(i);
    if i < list.len() {
        // SAFETY: every pointer in a generation list is a live object (`untrack` runs first thing
        // in `Drop for Object`).
        unsafe { (*list[i]).gc.idx.set(idx as u32) };
    }
}

/// Adds a freshly allocated container to generation 0.
pub fn track(rc: &Obj) {
    let o: &Object = rc;
    let p = Rc::as_ptr(rc);
    let _ = HEAP.try_with(|h| {
        let mut h = h.borrow_mut();
        h.seq = h.seq.wrapping_add(1);
        o.gc.seq.set(h.seq);
        let list = &mut h.lists[0];
        list.push(p);
        o.gc.idx.set(list.len() as u32);
        o.gc.gen.set(0);
        if h.sched.allocated() {
            DUE.with(|d| d.set(true));
        }
    });
}

/// Removes a dying object from its generation list.
pub fn untrack(o: &Object) {
    let idx = o.gc.idx.get() as usize;
    let gen = o.gc.gen.get() as usize;
    o.gc.idx.set(0);
    let _ = HEAP.try_with(|h| {
        let mut h = h.borrow_mut();
        h.sched.freed();
        remove_at(&mut h.lists[gen], idx);
    });
}

/// Moves a tracked object to generation `to`.
fn move_to(o: &Object, to: usize) {
    let idx = o.gc.idx.get() as usize;
    let from = o.gc.gen.get() as usize;
    if idx == 0 || from == to {
        return;
    }
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        remove_at(&mut h.lists[from], idx);
        let list = &mut h.lists[to];
        list.push(o as *const Object);
        o.gc.idx.set(list.len() as u32);
        o.gc.gen.set(to as u8);
    });
}

/// Takes a tuple that holds nothing collectable out of the lists (CPython's `untrack_tuples`).
fn untrack_if_atomic_tuple(o: &Object) -> bool {
    let Kind::Tuple(items) = &o.kind else { return false };
    if items.iter().any(|v| matches!(v, Value::Obj(x) if x.gc.idx.get() != 0)) {
        return false;
    }
    let idx = o.gc.idx.get() as usize;
    let from = o.gc.gen.get() as usize;
    o.gc.idx.set(0);
    o.gc.gen.set(UNTRACKED);
    HEAP.with(|h| remove_at(&mut h.borrow_mut().lists[from], idx));
    true
}

#[inline]
pub fn due() -> bool {
    DUE.with(|d| d.get())
}

pub fn bump_epoch() {
    EPOCH.with(|e| e.set(e.get() + 1));
}

pub fn is_finalizing() -> bool {
    FINALIZING.with(|f| f.get())
}

fn type_has_del(cls: &Obj) -> bool {
    let Kind::Type(td) = &cls.kind else { return false };
    let now = EPOCH.with(|e| e.get());
    let (epoch, cached) = td.del_cache.get();
    if epoch == now {
        return cached;
    }
    let h = hash_str("__del__");
    let mut found = false;
    if let Ok(mro) = td.mro.try_borrow() {
        for c in mro.iter() {
            let Ok(d) = c.dict.try_borrow() else { continue };
            let Some(Kind::Dict(dd)) = d.as_ref().map(|d| &d.kind) else { continue };
            let Ok(dd) = dd.try_borrow() else { continue };
            if let Some(i) = dd.find_str(h, "__del__") {
                found = !matches!(dd.get(i).map(|e| &e.val), Some(Value::None));
                break;
            }
        }
    }
    td.del_cache.set((now, found));
    found
}

/// Whether finalizing `o` runs something: a `__del__`, closing a suspended generator, or
/// the "never awaited" warning of a coroutine that never started.
fn wants_finalizer(o: &Object) -> bool {
    match &o.kind {
        Kind::Generator(gd) => match gd.state.try_borrow() {
            Ok(s) => match &*s {
                GenState::Suspended(_) => true,
                GenState::Created(_) => gd.kind == GenKind::Coroutine,
                _ => false,
            },
            Err(_) => false,
        },
        _ => o.cls.as_ref().is_some_and(type_has_del),
    }
}

/// Called from `Drop for Object` before anything is released: when the dying object has to be
/// finalized, moves it into a fresh allocation (same identity, weak references follow) and queues
/// that for the interpreter. Returns true when it did, and the caller then lets the husk go.
pub fn defer_finalizer(o: &mut Object) -> bool {
    if !wants_finalizer(o) {
        return false;
    }
    let id = o.id.replace(0);
    let moved = Object {
        cls: o.cls.take(),
        dict: RefCell::new(o.dict.get_mut().take()),
        id: Cell::new(id),
        gc: GcCell::new(),
        kind: std::mem::replace(&mut o.kind, Kind::Instance),
    };
    moved.gc.flags.set(GC_FINALIZED);
    let rc = Object::alloc(moved);
    if id != 0 {
        crate::weak::retarget(id, &rc);
    }
    let _ = FINALIZERS.try_with(move |f| f.borrow_mut().push(rc));
    crate::weak::mark_pending();
    true
}

/// The objects waiting for their finalizer to run.
pub fn take_finalizers() -> Vec<Obj> {
    FINALIZERS.try_with(|f| std::mem::take(&mut *f.borrow_mut())).unwrap_or_default()
}

// ---- the heap's public face (the `gc` module) -------------------------------------------------------

pub fn enabled() -> bool {
    HEAP.with(|h| h.borrow().enabled)
}

pub fn set_enabled(on: bool) {
    HEAP.with(|h| h.borrow_mut().enabled = on);
}

pub fn counts() -> [usize; GENERATIONS] {
    HEAP.with(|h| h.borrow().sched.count)
}

pub fn thresholds() -> [usize; GENERATIONS] {
    HEAP.with(|h| h.borrow().sched.threshold)
}

pub fn set_thresholds(t: [Option<usize>; GENERATIONS]) {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        for (slot, v) in h.sched.threshold.iter_mut().zip(t) {
            if let Some(v) = v {
                *slot = v;
            }
        }
    });
}

pub fn stats() -> [GenStats; GENERATIONS] {
    HEAP.with(|h| h.borrow().sched.stats)
}

pub fn generation_sizes() -> ([usize; GENERATIONS], usize) {
    HEAP.with(|h| {
        let h = h.borrow();
        ([h.lists[0].len(), h.lists[1].len(), h.lists[2].len()], h.lists[PERMANENT].len())
    })
}

fn upgrade(p: *const Object) -> Obj {
    // SAFETY: `p` came from `Rc::as_ptr` of an object that is still in a generation list, hence
    // alive with a strong count of at least one.
    unsafe {
        Rc::increment_strong_count(p);
        Rc::from_raw(p)
    }
}

/// Strong handles to the objects of the generations `gens` (oldest first).
fn members(gens: &[usize]) -> Vec<Obj> {
    let ptrs: Vec<*const Object> = HEAP.with(|h| {
        let h = h.borrow();
        gens.iter().flat_map(|&g| h.lists[g].iter().copied()).collect()
    });
    ptrs.into_iter().map(upgrade).collect()
}

/// `gc.is_tracked`: whether the collector looks at `o`. Dicts count as tracked only once they hold
/// something that may be tracked, as in CPython.
pub fn is_tracked(o: &Obj) -> bool {
    if o.gc.idx.get() == 0 {
        return false;
    }
    match &o.kind {
        Kind::Dict(d) => d.try_borrow().map(|d| d.iter().any(|e| holds_tracked(&e.key) || holds_tracked(&e.val))).unwrap_or(true),
        _ => true,
    }
}

fn holds_tracked(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if o.gc.idx.get() != 0)
}

pub fn is_finalized(o: &Obj) -> bool {
    o.gc.flags.get() & GC_FINALIZED != 0
}

/// `gc.get_objects`: the tracked objects of one generation, or of all three.
pub fn objects(generation: Option<usize>) -> Vec<Obj> {
    let gens: Vec<usize> = match generation {
        Some(g) => vec![g],
        None => vec![0, 1, 2],
    };
    members(&gens).into_iter().filter(is_tracked).collect()
}

/// `gc.get_referrers`: the tracked objects holding a reference to any of `targets`.
pub fn referrers(targets: &[Value]) -> Vec<Obj> {
    let wanted: Vec<*const Object> = targets.iter().filter_map(|v| v.as_obj().map(Rc::as_ptr)).collect();
    let mut out = Vec::new();
    for o in members(&[0, 1, 2, PERMANENT]) {
        let mut hit = false;
        gc_traverse::traverse(&o, &mut |t: &Obj| hit |= wanted.contains(&Rc::as_ptr(t)));
        if hit {
            out.push(o);
        }
    }
    out
}

/// `gc.freeze`: every tracked object moves to the permanent generation.
pub fn freeze() {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        for g in 0..PERMANENT {
            let moved = std::mem::take(&mut h.lists[g]);
            for p in moved {
                let list = &mut h.lists[PERMANENT];
                list.push(p);
                // SAFETY: a pointer in a generation list is a live object.
                unsafe {
                    (*p).gc.idx.set(list.len() as u32);
                    (*p).gc.gen.set(PERMANENT as u8);
                }
            }
            h.sched.count[g] = 0;
        }
    });
}

/// `gc.unfreeze`: the permanent generation joins the oldest one.
pub fn unfreeze() {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        let moved = std::mem::take(&mut h.lists[PERMANENT]);
        for p in moved {
            let list = &mut h.lists[2];
            list.push(p);
            // SAFETY: a pointer in a generation list is a live object.
            unsafe {
                (*p).gc.idx.set(list.len() as u32);
                (*p).gc.gen.set(2);
            }
        }
    });
}

// ---- the collection -------------------------------------------------------------------------------

/// Sorted pointer-to-index lookup of a snapshot.
struct Index(Vec<(usize, u32)>);

impl Index {
    fn new(objs: &[Obj]) -> Index {
        let mut keys: Vec<(usize, u32)> = objs.iter().enumerate().map(|(i, o)| (Rc::as_ptr(o) as usize, i as u32)).collect();
        keys.sort_unstable();
        Index(keys)
    }

    fn get(&self, p: *const Object) -> Option<u32> {
        let p = p as usize;
        self.0.binary_search_by_key(&p, |k| k.0).ok().map(|i| self.0[i].1)
    }
}

/// The reference counts and edges of a set of objects, for [`cycle::unreachable_mask`]. Every
/// object is held once more by the snapshot itself, which is not an outside reference.
struct Snap {
    strong: Vec<usize>,
    edges: Vec<Vec<u32>>,
}

impl Snap {
    fn build(objs: &[Obj]) -> Snap {
        let index = Index::new(objs);
        let strong = objs.iter().map(|o| Rc::strong_count(o).saturating_sub(1)).collect();
        let edges = objs
            .iter()
            .map(|o| {
                let mut e = Vec::new();
                gc_traverse::traverse(o, &mut |t: &Obj| {
                    if let Some(j) = index.get(Rc::as_ptr(t)) {
                        e.push(j);
                    }
                });
                e
            })
            .collect();
        Snap { strong, edges }
    }
}

impl Graph for Snap {
    fn len(&self) -> usize {
        self.strong.len()
    }

    fn strong(&self, n: usize) -> usize {
        self.strong[n]
    }

    fn edges(&self, n: usize, f: &mut dyn FnMut(usize)) {
        for &j in &self.edges[n] {
            f(j as usize);
        }
    }
}

/// The collector's per-interpreter state.
#[derive(Default)]
pub struct GcState {
    pub debug: u32,
    /// `gc.garbage`.
    pub garbage: Option<Value>,
    /// `gc.callbacks`.
    pub callbacks: Option<Value>,
}

fn begin_collecting() -> bool {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        !std::mem::replace(&mut h.collecting, true)
    })
}

fn end_collecting() {
    HEAP.with(|h| h.borrow_mut().collecting = false);
}

impl Interp {
    pub fn bump_type_epoch(&mut self) {
        self.type_epoch += 1;
        bump_epoch();
    }

    /// Runs a collection when allocations pushed generation 0 over its threshold (called from
    /// `poll`).
    pub fn gc_auto(&mut self) {
        DUE.with(|d| d.set(false));
        let due = HEAP.with(|h| {
            let h = h.borrow();
            if h.enabled && h.sched.threshold[0] != 0 && !h.collecting {
                h.sched.due()
            } else {
                None
            }
        });
        if let Some(g) = due {
            self.gc_collect_checked(g);
        }
    }

    /// `gc.collect(generation)`: the number of unreachable objects found, 0 when a collection is
    /// already running.
    pub fn gc_collect_checked(&mut self, generation: usize) -> usize {
        if !begin_collecting() {
            return 0;
        }
        self.gc_callbacks("start", generation, 0, 0);
        let (m, n) = self.gc_run(generation);
        self.gc_callbacks("stop", generation, m, n);
        end_collecting();
        self.run_weak_callbacks();
        m + n
    }

    /// A full collection without callbacks, for shutdown and for reclaiming memory.
    pub fn gc_collect_quiet(&mut self) -> usize {
        if !begin_collecting() {
            return 0;
        }
        let (m, n) = self.gc_run(GENERATIONS - 1);
        end_collecting();
        self.run_weak_callbacks();
        m + n
    }

    /// Before a `MemoryError` for the heap budget: whether collecting cycles may have freed some.
    pub fn gc_reclaim(&mut self) -> bool {
        enabled() && self.gc_collect_quiet() > 0
    }

    fn gc_callbacks(&mut self, phase: &str, generation: usize, collected: usize, uncollectable: usize) {
        let Some(Value::Obj(list)) = self.native_state::<GcState>().callbacks.clone() else { return };
        let cbs: Vec<Value> = match &list.kind {
            Kind::List(l) => l.borrow().clone(),
            _ => return,
        };
        if cbs.is_empty() {
            return;
        }
        let info = self.new_dict();
        dict_set_str(&info, "generation", Value::Int(generation as i64));
        dict_set_str(&info, "collected", Value::Int(collected as i64));
        dict_set_str(&info, "uncollectable", Value::Int(uncollectable as i64));
        for cb in cbs {
            let args = vec![Value::str(phase), Value::Obj(info.clone())];
            if let Err(e) = self.call(&cb, args, Vec::new()) {
                self.write_unraisable(&e, None, Some(&cb));
            }
        }
    }

    fn gc_debug_line(&mut self, text: String) {
        self.print_to_sys_stderr(&text);
    }

    /// `gc_collect_main`: collects generation `gen` (and the younger ones); returns the number of
    /// objects freed and of uncollectable ones.
    fn gc_run(&mut self, gen: usize) -> (usize, usize) {
        let debug = self.native_state::<GcState>().debug;
        let t0 = self.platform.borrow().monotonic_ns();
        if debug & DEBUG_STATS != 0 {
            let (sizes, perm) = generation_sizes();
            self.gc_debug_line(format!(
                "gc: collecting generation {gen}...\ngc: objects in each generation: {} {} {}\ngc: objects in permanent generation: {perm}\n",
                sizes[0], sizes[1], sizes[2]
            ));
        }
        HEAP.with(|h| h.borrow_mut().sched.begin(gen));

        let gens: Vec<usize> = (0..=gen).rev().collect();
        let mut cand = members(&gens);
        cand.retain(|o| !untrack_if_atomic_tuple(o));
        let snap = Snap::build(&cand);
        let unreachable = cycle::unreachable_mask(&snap);
        drop(snap);

        let mut garbage: Vec<Obj> = Vec::new();
        let mut survivors = 0usize;
        let target = (gen + 1).min(GENERATIONS - 1);
        for (o, dead) in cand.into_iter().zip(unreachable) {
            if dead {
                garbage.push(o);
            } else {
                survivors += 1;
                if gen < GENERATIONS - 1 {
                    move_to(&o, target);
                }
            }
        }
        garbage.sort_by_key(|o| o.gc.seq.get());
        for o in &garbage {
            o.gc.flags.set(o.gc.flags.get() | GC_UNREACH);
        }

        self.gc_handle_weakrefs(&garbage);

        for o in &garbage {
            if wants_finalizer(o) && o.gc.flags.get() & GC_FINALIZED == 0 {
                o.gc.flags.set(o.gc.flags.get() | GC_FINALIZED);
                self.finalize_object(o);
            }
        }

        let snap = Snap::build(&garbage);
        let still_dead = cycle::unreachable_mask(&snap);
        drop(snap);
        let mut doomed: Vec<Obj> = Vec::new();
        for (o, dead) in garbage.into_iter().zip(still_dead) {
            o.gc.flags.set(o.gc.flags.get() & !GC_UNREACH);
            if dead {
                doomed.push(o);
            } else {
                move_to(&o, target);
            }
        }

        let collected = doomed.len();
        let uncollectable = 0;
        if debug & DEBUG_COLLECTABLE != 0 {
            for o in &doomed {
                let line = self.gc_object_line("collectable", o);
                self.gc_debug_line(line);
            }
        }
        let keep: Vec<Weak<Object>> = doomed.iter().map(Rc::downgrade).collect();
        if debug & DEBUG_SAVEALL != 0 {
            if let Some(Value::Obj(list)) = self.native_state::<GcState>().garbage.clone() {
                if let Kind::List(l) = &list.kind {
                    l.borrow_mut().extend(doomed.iter().map(|o| Value::Obj(o.clone())));
                }
            }
        } else {
            for o in &doomed {
                gc_traverse::clear(o);
            }
        }
        drop(doomed);
        for w in keep {
            if let Some(o) = w.upgrade() {
                move_to(&o, target);
            }
        }

        HEAP.with(|h| h.borrow_mut().sched.end(gen, survivors, collected, uncollectable));
        if debug & DEBUG_STATS != 0 {
            let t1 = self.platform.borrow().monotonic_ns();
            let secs = t1.saturating_sub(t0) as f64 / 1e9;
            self.gc_debug_line(format!("gc: done, {} unreachable, {uncollectable} uncollectable, {secs:.4}s elapsed\n", collected + uncollectable));
        }
        (collected, uncollectable)
    }

    fn gc_object_line(&mut self, what: &str, o: &Obj) -> String {
        let v = Value::Obj(o.clone());
        let t = self.type_name_of(&v);
        format!("gc: {what} <{t} {:#x}>\n", self.id_of(&v))
    }

    /// `handle_weakrefs`: clears the weak references to the garbage and calls the callbacks of
    /// those that are not garbage themselves.
    fn gc_handle_weakrefs(&mut self, garbage: &[Obj]) {
        let mut calls: Vec<Obj> = Vec::new();
        for o in garbage {
            let id = o.id.get();
            if id == 0 {
                continue;
            }
            for wr in crate::weak::clear_refs(id) {
                if wr.gc.flags.get() & GC_UNREACH == 0 && crate::weak::has_callback(&wr) {
                    calls.push(wr);
                }
            }
        }
        for wr in calls {
            if let Some(cb) = crate::weak::take_callback(&wr) {
                if let Err(e) = self.call(&cb, vec![Value::Obj(wr.clone())], Vec::new()) {
                    self.write_unraisable(&e, None, Some(&cb));
                }
            }
        }
    }

    /// Runs the finalizer of `o`: its `__del__`, or the close (or never-awaited warning) of a
    /// generator.
    pub fn finalize_object(&mut self, o: &Obj) {
        if let Kind::Generator(gd) = &o.kind {
            self.finalize_generator(o, gd);
            return;
        }
        let Some(cls) = o.cls.clone() else { return };
        let Some(del) = self.lookup_mro(&cls, "__del__") else { return };
        if del.is_none() {
            return;
        }
        let this = Value::Obj(o.clone());
        let r = match self.bind_descr(&del, &this, &cls) {
            Ok(bound) => self.call(&bound, Vec::new(), Vec::new()),
            Err(e) => Err(e),
        };
        if let Err(e) = r {
            self.write_unraisable(&e, None, Some(&del));
        }
    }

    fn finalize_generator(&mut self, o: &Obj, gd: &GenData) {
        let created = matches!(&*gd.state.borrow(), GenState::Created(_));
        let this = Value::Obj(o.clone());
        if gd.kind == GenKind::Coroutine && created {
            *gd.state.borrow_mut() = GenState::Done;
            let name = gd.qualname.borrow().to_string();
            let msg = format!("coroutine '{name}' was never awaited");
            if let Err(e) = crate::builtins::warningsm::warn_category(self, "RuntimeWarning", &msg, 1) {
                self.write_unraisable(&e, None, Some(&this));
            }
            return;
        }
        if gd.kind == GenKind::AsyncGen && gd.hooks_inited.get() {
            let finalizer = self.native_state::<crate::builtins::genm::AsyncGenHooks>().finalizer.clone();
            if let Some(f) = finalizer {
                if let Err(e) = self.call(&f, vec![this.clone()], Vec::new()) {
                    self.write_unraisable(&e, None, Some(&f));
                }
                return;
            }
        }
        if let Err(e) = self.gen_close(o) {
            self.write_unraisable(&e, None, Some(&this));
        }
    }

    /// Runs the queued finalizers and weak-reference callbacks of objects that died since the
    /// last check.
    pub fn run_weak_callbacks(&mut self) {
        while crate::weak::has_pending() {
            let finalizers = take_finalizers();
            let dead = crate::weak::take_pending();
            for o in finalizers {
                self.finalize_object(&o);
            }
            for r in dead {
                if let Some(cb) = crate::weak::take_callback(&r) {
                    if let Err(e) = self.call(&cb, vec![Value::Obj(r.clone())], Vec::new()) {
                        self.write_unraisable(&e, None, Some(&cb));
                    }
                }
            }
        }
    }

    // ---- shutdown -----------------------------------------------------------------------------

    /// `Py_FinalizeEx` after the exit hooks ran: a collection, then the modules are torn down
    /// the way CPython does (`sys.modules` emptied, module dicts overwritten with `None` in
    /// reverse import order, `sys` and `builtins` last), collecting in between so `__del__`
    /// methods and weakref callbacks run in CPython's order.
    pub fn finalize_modules(&mut self) {
        FINALIZING.with(|f| f.set(true));
        self.gc_collect_quiet();

        let modules = self.modules.clone();
        let entries: Vec<(Value, Value)> = match &modules.kind {
            Kind::Dict(d) => d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
            _ => Vec::new(),
        };
        let sys = self.sys_module.clone();
        let mut weaklist: Vec<Weak<Object>> = Vec::new();
        for (name, m) in &entries {
            if let (Value::Obj(mo), Some(n)) = (m, name.as_obj()) {
                let special = sys.as_ref().is_some_and(|s| Rc::ptr_eq(s, mo)) || name.as_str() == Some("builtins");
                if matches!(mo.kind, Kind::Module) && !special {
                    weaklist.push(Rc::downgrade(mo));
                }
                dict_set_name(&modules, n, Value::None);
            }
        }
        drop(entries);
        if let Kind::Dict(d) = &modules.kind {
            let old = d.borrow_mut().take_all();
            drop(old);
        }
        self.main_globals = None;
        self.run_weak_callbacks();
        self.gc_collect_quiet();

        for w in weaklist.iter().rev() {
            if let Some(m) = w.upgrade() {
                let d = self.module_dict(&m);
                self.clear_module_dict(&d);
            }
        }
        drop(weaklist);
        self.gc_collect_quiet();
        crate::builtins::iom::flush_std_streams(self);

        if let Some(sys) = self.sys_module.clone() {
            let d = self.module_dict(&sys);
            for name in ["path", "argv", "meta_path", "path_hooks", "path_importer_cache"] {
                if let Value::Obj(n) = Value::str(name) {
                    dict_del_name(&d, &n);
                }
            }
            self.run_weak_callbacks();
            self.clear_module_dict(&d);
        }
        let builtins = self.builtins.clone();
        self.clear_module_dict(&builtins);
        self.gc_collect_quiet();
        self.run_weak_callbacks();
        self.flush_out();
    }

    /// `_PyModule_ClearDict`: names with a single leading underscore become `None` first, then
    /// everything except `__builtins__`.
    fn clear_module_dict(&mut self, d: &Obj) {
        let keys: Vec<Obj> = match &d.kind {
            Kind::Dict(dd) => dd.borrow().iter().filter_map(|e| e.key.as_obj().cloned()).collect(),
            _ => return,
        };
        let names: Vec<(Obj, String)> = keys.into_iter().filter_map(|k| k.kind_str().map(|s| (k.clone(), s))).collect();
        for pass in 0..2 {
            for (key, name) in &names {
                let hit = if pass == 0 {
                    name.starts_with('_') && !name.starts_with("__")
                } else {
                    name != "__builtins__"
                };
                if hit && dict_get_name(d, key).is_some() {
                    dict_set_name(d, key, Value::None);
                    self.run_weak_callbacks();
                }
            }
        }
    }
}

impl Object {
    fn kind_str(&self) -> Option<String> {
        match &self.kind {
            Kind::Str(s) => Some(s.s.to_string()),
            _ => None,
        }
    }
}
