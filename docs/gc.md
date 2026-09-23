# Heap and collector roadmap

Today every object is an `RcBox<RefCell<Object>>`. Every `Value` clone and drop touches a
refcount, every property access goes through a `RefCell` borrow check, every allocation
registers a raw slot in `GcRegistry`, and cycles are reclaimed by a trial-deletion pass over
the *whole* heap (`Interp::gc_collect`: `gc_snapshot` clones a handle to every live object,
counts internal edges, and roots anything whose strong count exceeds them). Profiles of
DeltaBlue, EarleyBoyer and the Djot parser are dominated by refcounting, allocation and
cycle collection, and those workloads are 20-40x behind Node (see `optimizer.md`).

The target is a traced, mostly non-moving, generational heap. Getting there must not break
the 170k lines of native Rust that hold object handles, so the migration is staged so each
step is behaviour-preserving and independently measurable.

## Rooting: counted native handles, uncounted heap edges

The existing collector has one property worth keeping: native code never registers roots.
A `Gc` held anywhere in Rust keeps its object alive because the collector sees
`strong > internal`. We keep that, but split the two kinds of reference:

- **`Gc` (native handle)**: counted. Held by Rust code, side tables, `Interp` fields,
  coroutine threads. A nonzero count makes the object a root. Dropping the last `Gc` does
  *not* free the object; it may still be reachable from the heap.
- **Heap edge**: an uncounted raw word stored inside the heap (property values, elements,
  prototype links, scope bindings, and eventually VM/JIT frames). Traced by the collector.

The collector's roots are therefore: objects with a nonzero native count, the VM register
files and JIT frames (scanned precisely: they are `Value` arrays we own), and the `Interp`
root set. Everything else is found by tracing. This removes refcount traffic from every heap
load/store and from the VM/JIT hot paths, while native Rust keeps compiling unchanged.

Satori's answer to the same problem (native pointers the collector can't describe precisely)
is conservative roots that pin their whole region. We use counts instead because Rust also
keeps handles in heap-allocated containers (`Vec<Value>`, `HashMap`s) that no stack scan
sees.

## Heap layout (from Satori)

- 2 MB aligned **regions**, metadata inside the region: a mark bitmap (1 bit per 16-byte
  granule), an object-start index, per-region free lists. `ptr & !(2MB-1)` finds the region;
  a page map answers "is this a heap pointer?".
- **Allocation** bump-allocates from the current region; a swept region's free list is
  reused before a fresh region is taken. Large objects (> 32 KB) get their own regions and
  never move.
- **Sweeping is lazy**: a region is swept when the allocator next wants it, not in the pause.
- **Footprint**: empty regions are returned to the OS by a rate-limited trimmer after a full
  collection, backing off while collections are frequent.

## Generations

Non-moving generational with **sticky mark bits**: a minor collection marks from roots plus
the remembered set and treats every already-marked (old) object as live. A region is
promoted whole once it survives, so no copying is needed.

The write barrier (JIT-emitted, and in `Props` mutation) follows Satori's filtering design:
store first, then only if the source object is old and the stored value is a young heap
pointer, dirty a card (one byte per 512 bytes, with a per-region summary byte). Check-then-
write, no atomics. A global "next GC is full" state lets the barrier skip cards entirely.

Not taken from Satori: thread-local gen0 with escape tracking and concurrent marking. Both
exist to avoid stopping other mutator threads; lumen has one mutator per heap. If pause
times ever matter, time-sliced incremental marking at allocation checkpoints (with a
dirtying barrier) is the variant that fits.

## Triggers

A minor collection after the nursery budget is allocated; the budget tracks survival
(Satori: `live * (target - 1)`, floor 4 MB, smoothed). A full collection when the heap is
projected past 2x the live size after the last full collection, and at least once every
N minors.

## Stages

1. **Opaque handle** (done): `Gc` and `WeakGc` are newtypes; no engine code names
   `Rc<RefCell<Object>>` or calls `Rc::*` on an object. The stored word is unchanged, so the
   JIT's measured layout still holds.
2. **Own box**: replace the inner `Rc` with a lumen-owned `GcBox { strong, weak, value }`
   with `RcBox`'s layout. No behaviour change; allocation now goes through code we control,
   which allows the region allocator and removes `GcRegistry` (regions are iterable).
3. **Region allocator**: objects allocated from regions; sweep by bitmap. Still refcounted.
4. **Uncounted heap edges**: `PackedValue` in `Props` and elements stores raw pointers;
   `Gc` counts become native-only; the collector traces. Objects are freed by sweep, not by
   the last `Drop`. Scopes (`Env`) get the same treatment.
5. **Uncounted frames**: VM register files and JIT frames hold raw words and are scanned
   precisely; the JIT's inline refcount templates go away.
6. **Generations and barrier**: sticky mark bits, cards, JIT barrier.
7. **Side tables into objects**: `map_data`, `typed_arrays`, `regexps`, `proxies`,
   `promises` and friends are pointer-keyed hash maps on `Interp`, so every `Map.get` pays a
   hash lookup before the real one. Store the payload in the object (`Exotic` payload) so the
   collector traces it and the sweep no longer evicts eleven maps per dead object.

Each stage lands with test262 unchanged and a V8-v7/Djot comparison against the previous
stage.
