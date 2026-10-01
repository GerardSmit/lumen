//! The cycle collector.
//!
//! An object whose strong count exceeds the references it receives from other heap nodes has an
//! *external* holder — the Rust stack, a scope, the global, a side table — so it and everything
//! it reaches is live; everything else is referenced only from unreachable cycles and is
//! reclaimed by breaking its references. No root enumeration is needed, so a collection may run
//! in the middle of evaluation.
//!
//! The collector allocates almost nothing: it walks the slab chunks and the scope registry in
//! place, keeps its per-node scratch inside the nodes (an object's `gc_internal` word, 16 bits
//! of padding in a scope's binding map), visits edges through callbacks instead of collecting
//! them, and marks with a stack of raw pointers that steps through large property maps in
//! bounded chunks. Only the garbage itself is held (strongly) across the sweep, so its
//! destructors cannot free a node whose side-table entries are still to be evicted.
use super::{Callable, Env, Exotic, Interp, Scope};
use crate::fasthash::FastMap;
use crate::value::gc_edges::{visit_object_head, visit_object_refs};
use crate::value::{self, Gc, ObjCell, Value};
use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

type ScopePtr = *const RefCell<Scope>;

const SCOPE_MARK: u16 = 1 << 15;
const SCOPE_COUNT_MAX: u16 = SCOPE_MARK - 1;

/// Property slots one mark step visits before yielding, so the pending stack of a million-element
/// array stays small.
const MARK_STEP: usize = 1024;

/// Set while a collection runs: one that unwound leaves node scratch dirty, which the next
/// collection (on any thread) clears before trusting it.
static IN_COLLECTION: AtomicBool = AtomicBool::new(false);

fn reset_scratch() {
    value::gc_for_each_live(|o| o.gc_reset());
    value::scope_registry_with(|reg| {
        for w in reg {
            // SAFETY: the registry holds only live scopes.
            unsafe { scope_scratch(w.as_ptr()) }.set(0);
        }
    });
}

/// The scratch word of a scope.
///
/// # Safety
/// `p` points at a live scope.
#[inline]
unsafe fn scope_scratch<'a>(p: ScopePtr) -> &'a Cell<u16> {
    (*(*p).as_ptr()).vars.gc_scratch()
}

/// Whether the scope is in the registry the collector walks (`register_scope` numbers it).
///
/// # Safety
/// `p` points at a live scope.
#[inline]
unsafe fn scope_registered(p: ScopePtr) -> bool {
    (*(*p).as_ptr()).serial != 0
}

enum Edge<'a> {
    Obj(&'a Gc),
    Scope(&'a Env),
}

fn visit_scope_edges(scope: &Scope, f: &mut impl FnMut(Edge)) {
    if let Some(parent) = &scope.parent {
        f(Edge::Scope(parent));
    }
    if let Some(Value::Obj(o)) = scope.with_obj() {
        f(Edge::Obj(o));
    }
    for bind in scope.vars.values() {
        if let Value::Obj(o) = &bind.value {
            f(Edge::Obj(o));
        }
    }
    for import in scope.import_envs() {
        f(Edge::Scope(import));
    }
}

/// Internal reference counts of scopes: 15 bits in the scope itself, the rest (a scope shared
/// by tens of thousands of closures) here.
#[derive(Default)]
struct ScopeCounts {
    overflow: FastMap<usize, u32>,
}

impl ScopeCounts {
    #[inline]
    fn add(&mut self, p: ScopePtr) {
        // SAFETY: edges lead to live scopes.
        unsafe {
            if !scope_registered(p) {
                return;
            }
            let c = scope_scratch(p);
            let v = c.get();
            if v & SCOPE_COUNT_MAX == SCOPE_COUNT_MAX {
                *self.overflow.entry(p as usize).or_insert(0) += 1;
            } else {
                c.set(v + 1);
            }
        }
    }

    fn get(&self, p: ScopePtr) -> usize {
        // SAFETY: `p` is a live registry entry.
        let n = unsafe { scope_scratch(p).get() & SCOPE_COUNT_MAX } as usize;
        if n == SCOPE_COUNT_MAX as usize {
            n + self.overflow.get(&(p as usize)).copied().unwrap_or(0) as usize
        } else {
            n
        }
    }
}

#[derive(Default)]
struct Tracer {
    objects: Vec<*const ObjCell>,
    wide: Vec<(*const ObjCell, usize)>,
    scopes: Vec<ScopePtr>,
    marked: usize,
}

impl Tracer {
    #[inline]
    fn mark_obj(&mut self, g: &Gc) {
        if !g.gc_marked() {
            g.gc_set_mark();
            self.marked += 1;
            self.objects.push(Gc::as_ptr(g));
        }
    }

    #[inline]
    fn mark_scope(&mut self, p: ScopePtr) {
        // SAFETY: edges lead to live scopes.
        unsafe {
            if !scope_registered(p) {
                return;
            }
            let c = scope_scratch(p);
            let v = c.get();
            if v & SCOPE_MARK == 0 {
                c.set(v | SCOPE_MARK);
                self.scopes.push(p);
            }
        }
    }
}

impl Interp {
    /// The scopes `o` refers to: a user function's closure environment, a mapped `arguments`
    /// object's aliased parameter scope, and a class constructor's field-initializer environment.
    fn visit_object_scopes(&self, o: &Gc, f: &mut impl FnMut(&Env)) {
        // Only a user function (a class constructor) can have `class_info`, and only an
        // arguments exotic object `mapped_arguments`: the rest of the heap skips both probes.
        let (user, arguments) = {
            let b = o.borrow();
            let user = if let Callable::User(user) = &b.call {
                f(&user.env);
                true
            } else {
                false
            };
            (user, matches!(b.exotic, Exotic::Arguments))
        };
        if !(user && !self.class_info.is_empty() || arguments && !self.mapped_arguments.is_empty())
        {
            return;
        }
        let ptr = Gc::as_ptr(o) as usize;
        if let Some((env, _)) = self.mapped_arguments.get(&ptr) {
            f(env);
        }
        if let Some(ci) = self.class_info.get(&ptr) {
            f(&ci.field_env);
        }
    }

    fn trace_object(&self, p: *const ObjCell, tr: &mut Tracer, weak_live: &mut Vec<usize>) {
        // SAFETY: a marked object is alive and nothing is released during marking.
        let o = ManuallyDrop::new(unsafe { Gc::from_raw(p) });
        {
            let b = o.borrow();
            visit_object_head(&b, true, &mut |c| tr.mark_obj(c));
            if let Some(next) = b
                .props
                .visit_object_refs(0, MARK_STEP, &mut |c| tr.mark_obj(c))
            {
                tr.wide.push((p, next));
            }
        }
        if !self.map_data.is_empty() {
            let ptr = p as usize;
            if let Some(data) = self.map_data.get(&ptr) {
                use crate::builtins::collection_data::CollectionKind as K;
                if matches!(data.kind(), K::WeakMap | K::WeakSet) {
                    weak_live.push(ptr);
                } else {
                    for (k, v) in data.iter() {
                        for x in [k, v] {
                            if let Value::Obj(c) = &*x.get() {
                                tr.mark_obj(c);
                            }
                        }
                    }
                }
            }
        }
        self.visit_object_scopes(&o, &mut |e| tr.mark_scope(Rc::as_ptr(e)));
        if !self.proxies.is_empty() {
            if let Some((t, h)) = self.proxies.get(&(p as usize)) {
                for v in [t, h] {
                    if let Value::Obj(c) = v {
                        tr.mark_obj(c);
                    }
                }
            }
        }
        if self.multi_realm() {
            if let Some(r) = self.realms.get(&(p as usize)).filter(|r| r.collectable) {
                r.for_each_object(|c| tr.mark_obj(c));
                tr.mark_scope(Rc::as_ptr(&r.global_env));
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn gc_collect(&mut self) {
        self.gc_collect_cycles();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn gc_collect(&mut self) {
        use crate::gc_log::{GcEvent, GcObserver};
        let Some(epoch) = self.host_state.get::<GcObserver>().map(|o| o.epoch) else {
            return self.gc_collect_cycles();
        };
        let start = std::time::Instant::now();
        self.gc_collect_cycles();
        let duration = start.elapsed();
        let Some(observer) = self.host_state.get_mut::<GcObserver>() else { return };
        let forced = std::mem::take(&mut observer.forced_next);
        observer.events.push(GcEvent { start: start.saturating_duration_since(epoch), duration, forced });
        if !std::mem::replace(&mut observer.queued, true) {
            let callback = observer.callback.clone();
            self.queue_microtask(callback);
        }
    }

    fn gc_collect_cycles(&mut self) {
        self.str_units.clear();
        super::GC_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Count references between heap nodes (objects and scopes). Scopes are nodes too: a
        // closure's captured environment references objects (its bindings) and vice versa
        // (`Callable::User`), so cycles routinely pass through them. The scratch of every node
        // is zero here: collections and allocation leave it so.
        if IN_COLLECTION.swap(true, Ordering::Relaxed) {
            reset_scratch();
        }
        let mut counts = ScopeCounts::default();
        value::gc_for_each_live(|o| {
            visit_object_refs(&o.borrow(), &mut |c| c.gc_add_ref());
            self.visit_object_scopes(o, &mut |e| counts.add(Rc::as_ptr(e)));
        });
        // The first registry walk purges dead entries; the later ones rely on that.
        value::scope_registry_with(|reg| {
            for w in reg {
                // SAFETY: the registry holds only live scopes and nothing releases one here.
                let b = unsafe { &*w.as_ptr() }.borrow();
                visit_scope_edges(&b, &mut |edge| match edge {
                    Edge::Obj(o) => o.gc_add_ref(),
                    Edge::Scope(e) => counts.add(Rc::as_ptr(e)),
                });
            }
        });
        // A pin is bookkeeping, not a real holder: count it like an internal reference so a
        // pinned-but-unreachable object is still collectable (the sweep evicts its entries).
        for o in self.gc_pins.values() {
            o.gc_add_ref();
        }
        // A proxy's target and handler are edges of the proxy object (traced when it is marked),
        // so a cycle through them (a `vm` context's global proxy) is collectable.
        for (t, h) in self.proxies.values() {
            for v in [t, h] {
                if let Value::Obj(o) = v {
                    o.gc_add_ref();
                }
            }
        }
        // A `vm` context realm's registry entry is bookkeeping too: it is traced from the realm's
        // global and dropped with it.
        let vm_realms = self.multi_realm() && self.realms.values().any(|r| r.collectable);
        if vm_realms {
            // Per-realm `arguments` templates are rebuilt on demand; they must not pin a realm.
            self.args_tpls.clear();
            for r in self.realms.values().filter(|r| r.collectable) {
                r.for_each_object(|o| o.gc_add_ref());
                counts.add(Rc::as_ptr(&r.global_env));
            }
        }
        // A Map/Set/WeakMap/WeakSet's entries are edges of the collection object (the table is
        // its [[MapData]] slot), not external holders: a collection in a cycle, and whatever a
        // weak collection holds, must not root its entries. Strong collections trace them when
        // marked; weak ones as ephemerons (below).
        for data in self.map_data.values() {
            // A Set holds one reference per entry: its value is the key itself.
            let values = data.has_values();
            for (k, v) in data.iter() {
                if let Value::Obj(o) = &*k.get() {
                    o.gc_add_ref();
                }
                if let (true, Value::Obj(o)) = (values, &*v.get()) {
                    o.gc_add_ref();
                }
            }
        }

        // Roots: nodes with a reference from outside the heap graph (the Rust call stack, the
        // Interp's own fields, module/realm registries, coroutine threads).
        let mut tr = Tracer::default();
        value::gc_for_each_live(|o| {
            if Gc::strong_count(o) > o.gc_refs() as usize {
                tr.mark_obj(o);
            }
        });
        value::scope_registry_view(|reg| {
            for w in reg {
                let p = w.as_ptr();
                if w.strong_count() > counts.get(p) {
                    tr.mark_scope(p);
                }
            }
        });
        if std::env::var_os("LUMEN_GC_DUMP").is_some() {
            self.gc_dump_roots(&counts);
        }
        drop(counts);

        // Mark everything reachable from the roots, across both node types. Live weak
        // collections are gathered for the ephemeron pass: a WeakMap value (a WeakSet member)
        // is reachable only while its key is, however the value refers back to the key.
        let mut weak_live: Vec<usize> = Vec::new();
        loop {
            if let Some(p) = tr.objects.pop() {
                self.trace_object(p, &mut tr, &mut weak_live);
                continue;
            }
            if let Some((p, from)) = tr.wide.pop() {
                // SAFETY: as in `trace_object`.
                let o = ManuallyDrop::new(unsafe { Gc::from_raw(p) });
                let next = o
                    .borrow()
                    .props
                    .visit_object_refs(from, MARK_STEP, &mut |c| tr.mark_obj(c));
                if let Some(next) = next {
                    tr.wide.push((p, next));
                }
                continue;
            }
            if let Some(p) = tr.scopes.pop() {
                // SAFETY: a marked scope is alive and nothing is released during marking.
                let b = unsafe { &*p }.borrow();
                visit_scope_edges(&b, &mut |edge| match edge {
                    Edge::Obj(o) => tr.mark_obj(o),
                    Edge::Scope(e) => tr.mark_scope(Rc::as_ptr(e)),
                });
                continue;
            }
            // Ephemerons: mark the values of live weak collections whose keys are marked,
            // then resume marking from them until nothing changes.
            for ptr in &weak_live {
                for (k, v) in self.map_data[ptr].iter() {
                    let key_live = match &*k.get() {
                        Value::Obj(ko) => ko.gc_marked(),
                        _ => true,
                    };
                    if let (true, Value::Obj(vo)) = (key_live, &*v.get()) {
                        tr.mark_obj(vo);
                    }
                }
            }
            if tr.objects.is_empty() {
                break;
            }
        }
        drop(tr.objects);
        drop(tr.wide);
        drop(tr.scopes);
        let marked = tr.marked;
        // Realms whose global died go with their objects (held until the sweep is over).
        let mut dead_realms = Vec::new();
        if vm_realms {
            let dead: Vec<usize> = self
                .realms
                .iter()
                .filter(|(_, r)| r.collectable && !r.global.gc_marked())
                .map(|(k, _)| *k)
                .collect();
            for k in dead {
                if let Some(r) = self.realms.remove(&k) {
                    if let Some(ef) = &r.eval_fn {
                        self.eval_realm_fns.remove(&(Gc::as_ptr(ef) as usize));
                    }
                    dead_realms.push(r);
                }
            }
        }

        // Entries of live weak collections whose key died go now: the key (and a value only
        // it kept alive) is swept below, and nothing may observe it through the table. The
        // removed entries are held until the sweep is over: a garbage object must not be freed
        // before its side-table entries are evicted.
        let mut removed: Vec<(Value, Value)> = Vec::new();
        for ptr in &weak_live {
            let data = self.map_data.get_mut(ptr).unwrap();
            let first = removed.len();
            removed.extend(
                data.iter()
                    .filter(|(k, _)| matches!(&*k.get(), Value::Obj(ko) if !ko.gc_marked()))
                    .map(|(k, v)| (k.unpack(), v.unpack())),
            );
            for (k, _) in &removed[first..] {
                data.remove(k);
            }
        }

        // Collect the garbage, holding it strongly, and reset the scratch of every node for the
        // next collection.
        let mut garbage: Vec<Gc> =
            Vec::with_capacity((value::live_objects().max(0) as usize).saturating_sub(marked));
        value::gc_for_each_live(|o| {
            if !o.gc_marked() {
                garbage.push(o.clone());
            }
            o.gc_reset();
        });
        let mut dead_scopes: Vec<Env> = Vec::new();
        value::scope_registry_view(|reg| {
            for w in reg {
                // SAFETY: the registry holds only live scopes.
                let c = unsafe { scope_scratch(w.as_ptr()) };
                if c.get() & SCOPE_MARK == 0 {
                    dead_scopes.extend(w.upgrade());
                }
                c.set(0);
            }
        });

        // Sweep: clear unmarked (garbage) objects to break their cycles; once `garbage` drops,
        // their refcounts hit zero and they are freed. Also evict them from pointer-keyed side
        // tables so a future object reusing the address can't inherit stale metadata.
        #[cfg(not(target_arch = "wasm32"))]
        let garbage_count = garbage.len();
        for o in &garbage {
            let ptr = Gc::as_ptr(o) as usize;
            // Most tables are empty in most programs: skip them without hashing.
            macro_rules! evict {
                ($($t:ident),*) => {$(
                    if !self.$t.is_empty() {
                        self.$t.remove(&ptr);
                    }
                )*};
            }
            evict!(
                class_info,
                map_data,
                typed_arrays,
                data_views,
                regexps,
                proxies,
                temporal,
                array_buffers,
                ta_buffer,
                shared_buffers,
                immutable_buffers,
                generators,
                async_gens,
                async_gen_busy,
                async_gen_queue,
                mapped_arguments,
                deferred_ns,
                module_ns,
                gc_pins
            );
            let mut b = o.borrow_mut();
            b.props.clear();
            b.proto = None;
            b.call = Callable::None;
            b.exotic = Exotic::None;
        }
        // Sweep garbage scopes the same way: emptying them breaks env-involving cycles.
        for e in &dead_scopes {
            let imports = {
                let mut b = e.borrow_mut();
                b.vars.clear();
                b.parent = None;
                b.clear_rare_edges()
            };
            drop(imports);
        }
        // Release the garbage first: only then do swept objects and their property buffers
        // reach the allocator's free lists. A high threshold confines the expensive platform
        // pressure-relief call to phase changes, not ordinary generational churn.
        IN_COLLECTION.store(false, Ordering::Relaxed);
        drop(garbage);
        drop(dead_scopes);
        drop(removed);
        drop(dead_realms);
        // FinalizationRegistry targets may have died: the next checkpoint scans for them.
        self.weak_note_collection();
        value::gc_trim_heap();
        // Function bodies that ran (a module initialiser, a one-shot setup path) and then sat
        // untouched for a whole collection interval are released here and re-parsed if they
        // are ever called again. This runs on every collection rather than only the ones that
        // trim: the pass is a flat loop over one `Weak` per lazily-parsed function (sub-
        // millisecond at 100k entries), and the bodies it frees are the bulk of a bundle's
        // retained AST, which no other pass would reclaim.
        let flushed = value::flush_cold_lazy_bodies();
        if flushed > 0 && std::env::var_os("LUMEN_GC_LOG").is_some() {
            eprintln!("[gc] released {flushed} cold function bodies");
        }
        if std::env::var_os("LUMEN_HEAP_CENSUS").is_some() {
            self.heap_census();
        }
        // Rate-limited: a steady cyclic-garbage churn collects every few milliseconds, and
        // draining the allocator cache each time turned every later allocation into a system
        // heap call until the cache refilled.
        #[cfg(not(target_arch = "wasm32"))]
        if (garbage_count >= 50_000 || flushed >= 512) && value::gc_allocator_trim_due() {
            crate::fastalloc::trim();
        }
        // An OOM-killed guest cannot print the normal-exit memory report. Opt-in
        // checkpoints expose retained structures while the process is still alive.
        if crate::memstats::enabled() && std::env::var_os("LUMEN_MEM_GC").is_some() {
            self.mem_report();
        }
    }

    /// Debug: `LUMEN_GC_DUMP=1` prints each external-rooted node's shape (its first prop names)
    /// with strong/internal counts — the fastest way to see WHAT pins a leaked graph.
    fn gc_dump_roots(&self, counts: &ScopeCounts) {
        let mut shown = 0;
        value::gc_for_each_live(|o| {
            if !o.gc_marked() || shown >= 60 {
                return;
            }
            let b = o.borrow();
            let keys: Vec<Rc<str>> = b.props.iter().take(4).map(|(k, _)| k).collect();
            eprintln!(
                "[gc-dump] root strong={} internal={} props={keys:?}",
                Gc::strong_count(o),
                o.gc_refs(),
            );
            shown += 1;
        });
        let mut shown = 0;
        value::scope_registry_view(|reg| {
            for w in reg {
                let internal = counts.get(w.as_ptr());
                let strong = w.strong_count();
                if strong <= internal || shown >= 40 {
                    continue;
                }
                let b = unsafe { &*w.as_ptr() }.borrow();
                let vars: Vec<&str> = b.vars.keys().take(6).map(|k| &**k).collect();
                eprintln!("[gc-dump] scope-root strong={strong} internal={internal} vars={vars:?}");
                shown += 1;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(interp: &mut Interp, src: &str) {
        let body = crate::parser::parse_script(src, false).expect("parse");
        assert!(interp.run_program_parsed(&body).is_ok(), "script threw");
    }

    /// Side-table names with an entry whose key is no live object. `regexps` is exempt: its
    /// entries are tied to a weak handle and pruned lazily.
    fn stale_side_tables(interp: &Interp) -> Vec<&'static str> {
        let mut live = std::collections::HashSet::new();
        value::gc_for_each_live(|o| {
            live.insert(Gc::as_ptr(o) as usize);
        });
        let mut stale = Vec::new();
        macro_rules! check {
            ($($table:ident),*) => {$(
                if interp.$table.keys().any(|k| !live.contains(k)) {
                    stale.push(stringify!($table));
                }
            )*};
        }
        check!(
            class_info,
            map_data,
            typed_arrays,
            data_views,
            proxies,
            temporal,
            array_buffers,
            ta_buffer,
            shared_buffers,
            generators,
            mapped_arguments,
            deferred_ns,
            module_ns,
            gc_pins
        );
        stale
    }

    /// A sweep frees a marked object only when a garbage object's own entry or closure was its
    /// sole holder; such an object has no entries of its own, because anything with a side-table
    /// entry is pinned (`gc_pin`) and so dies only in a sweep that evicts the entries first.
    #[test]
    fn objects_freed_with_their_garbage_holder_leave_no_side_table_entries() {
        let mut interp = Interp::new();
        run(
            &mut interp,
            "(function () {
               for (let round = 0; round < 3; round++) {
                 for (let i = 0; i < 200; i++) {
                   const m = new Map([[i, i]]), s = new Set([m]), w = new WeakMap([[m, s]]);
                   const px = new Proxy(m, {});
                   const ta = new Uint8Array(new ArrayBuffer(8));
                   const dv = new DataView(ta.buffer);
                   const arrayBuffer = new ArrayBuffer(4), view = new Int8Array(arrayBuffer);
                   function* g() { yield px; }
                   const it = g(); it.next();
                   class C { static #p = m; x = s; }
                   const args = (function (a) { return arguments; })(1);
                   const bound = function () {}.bind(null, m, px, ta, dv);
                   const hold = { m, s, w, px, ta, dv, it, C, args, bound, view };
                   hold.self = hold; m.set('hold', hold); px.hold = hold;
                 }
                 $262.gc();
               }
             })();",
        );
        for _ in 0..3 {
            interp.gc_collect();
        }
        assert_eq!(stale_side_tables(&interp), Vec::<&str>::new());
    }
}
