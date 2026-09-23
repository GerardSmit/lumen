# RyuJIT optimizations for lumen's JIT: prioritized todo

This list compares the .NET JIT (dotnet/runtime `src/coreclr/jit`, read at HEAD in 2026-09) with
lumen's optimizing tier (`crates/lumen/src/bytecode/jit/`, `crates/lumen-codegen/`). It
complements [jit.md](jit.md), which has the roadmap, and `docs/jit-notes/`, which has the RyuJIT
notes on LSRA, lowering and tiering. Each .NET idea is translated into its JavaScript-engine
equivalent. RyuJIT can rely on static types. lumen has to speculate with guards and exit to the
interpreter when a guard fails.

**Legend.**
- **Impact** is the expected effect on JS workloads (V8-v7, Djot, DeltaBlue-style OO code):
  **H** = high, **M** = medium, **L** = low.
- **Size**: **S** is under a week, **M** is 1–3 weeks, **L** is more than 3 weeks.
- **Deps** lists the items that must land first.
- **Ref** is the .NET file or function. Paths are relative to `src/coreclr/jit` unless they
  point into `docs/design/coreclr/jit`.
- An unchecked box means the item is not done. *Partial* says what already exists.

## Where lumen is today (so nothing below re-proposes it)

| Area | Status |
|---|---|
| Tier shape | Loop-only regions `[header, backedge]` are entered by OSR at the header. Leaving goes through an exit word; the interpreter resumes with a materialized stack. There is no whole-function tier and no inlining. |
| Representation | `Kind::{Num (F64), Bool (I32), Boxed}`, chosen from slot values at OSR time and checked by entry guards. **There is no Int32 kind.** Bit operations use a branch-free `to_int32` of F64. |
| Speculation | Per-site guards branch to *exit* blocks that write back and materialize state. Every exit is its own block. There is no deopt metadata, no invalidation, and no call-target feedback. |
| Feedback | The only feedback is monomorphic property ICs (`Chunk::caches`, holding `(proto depth, slot)`) and name/capture caches. There are no branch counts, no call-target profiles and no arithmetic-type profiles. |
| IR opts (`opt.rs`) | Unreachable-block removal, trivial block-parameter removal (copy prop), dominator-scoped GVN with constant folding, algebraic identities and branch/trap folding, and DCE. GVN covers **pure ops only**: it has no memory model, so loads are never CSE'd. |
| Translator facts (`build.rs`) | "Region caches" (binding addresses, `Math` guards) and a bounds-check-elimination (BCE) fact for the loop test `i < a.length`. All of them are reset whenever JS may run or the slot is written. This is a pattern-level partial form of assertion propagation. |
| Intrinsics | `Math.*` (sqrt, abs, floor, ceil, round, trunc, min, max, imul). Dense-array and string `.length`. Shape-guarded property loads baked from ICs (`layout.rs`). |
| Lowering | Immediate containment. Compare fused into the branch or `select` (jcc/b.cond/csel). `iadd` folded into the address mode. Free `uext`. |
| Regalloc | Linear scan. Each value has one location for its whole lifetime. Spills use single-def store and reload-at-use. Spill cost is `8^loop_depth`. Move and fixed-register hints. No splitting. |
| wasm32 | Ramsey-style structured control flow, a dispatch loop for irreducible CFGs, expression nesting of single-use pure values, and one wasm local per SSA value. |
| Safepoints | A budget counter is decremented at every backedge, with a helper call when it runs out. This is equivalent to RyuJIT's GC-poll/OSR counter. |

## Top 10, in order

1. **Whole-function optimizing tier with shared deopt exits and frame states**
   (see [T1](#t1) and [T2](#t2)). Every item below is capped by loop-only regions and inline
   exit blocks.
2. **Feedback recording in the interpreter: call targets, branch counts, operand kinds**
   ([T3](#t3)). This is the input to GDV, inlining, layout and unrolling decisions.
3. **Int32 speculation kind plus overflow guards** ([J1](#j1)). Most JS loop counters, indices
   and bit operations are int32. F64-only code pays for conversions and cannot use integer
   addressing.
4. **Inlining of monomorphic callees behind callee guards** ([J2](#j2) and [J3](#j3)). This is
   the single largest win for OO/closure-heavy code (DeltaBlue, Djot renderers). Today every JS
   call is a `Helper::Call` round trip through the interpreter.
5. **Memory-aware GVN / load CSE plus redundant guard elimination** ([G1](#g1) and [G2](#g2)).
   Shape checks, `.length` loads and slot loads repeat constantly in JS.
6. **Loop-invariant code motion (LICM) of guards and loads, with loop inversion and preheaders**
   ([G4](#g4) and [G3](#g3)).
7. **Range analysis and general bounds-check elimination (BCE)** ([G5](#g5)). This replaces
   today's single `i < a.length` pattern.
8. **Escape analysis and scalar replacement** of object literals, `arguments`, iterator
   results and short-lived closures ([J5](#j5)). This is RyuJIT's object stack allocation plus
   physical promotion, and it covers `for-of` de-abstraction.
9. **Refcount-traffic elision** ([J6](#j6)). This is lumen's equivalent of write-barrier
   elision: clone/drop pairs around Boxed values dominate helper-heavy loops.
10. **Profile-guided block layout plus cold exit placement** ([B6](#b6)), then **loop
    unrolling** ([G7](#g7)) once inlining and Int32 exist. Unrolling alone rarely pays in JS;
    see G7.

---

## T. Tier infrastructure (prerequisites)

- [ ] <a id="t1"></a>**T1. Whole-function compilation tier** — *Impact H, Size L*
  - **Ref:** tiered compilation (tier0 → tier1) and the OSR method variant
    (`patchpoint.cpp`, `fgTransformPatchpoints`, `TC_OnStackReplacement*` in
    `jitconfigvalues.h`). .NET compiles an optimized whole method and uses OSR only to escape
    long-running tier-0 loops.
  - **lumen:** hot functions compile whole, starting from the function entry. Keep today's loop
    regions as OSR entries into the same compiled function: the OSR entry block takes the live
    slots as block parameters, as the notes in `docs/jit-notes/opts-simd-tiering.md` suggest.
  - **Lives in:** `bytecode/jit/build.rs`, extended from region to function, plus a call-entry
    stub. `mod.rs` needs call counters next to the backedge ticks.
  - **Why:** without it, inlining, return-value speculation, and anything that crosses a call
    boundary is impossible, and short hot functions never compile at all.
  - **Deps:** T2.

- [ ] <a id="t2"></a>**T2. Shared deopt exits with frame-state metadata** — *Impact H, Size M*
  - **Ref:** RyuJIT has no equivalent (static types). The closest idea is throw-helper block
    sharing: `fgGetExcptnTarget` / `fgUseThrowHelperBlocks` put one throw block per kind per EH
    region, out of line.
  - **lumen:** today each guard emits its own exit block that writes back locals and
    materializes the stack. Replace that with a guard that branches to a per-(pc, state) shared
    stub or to a single deopt trampoline indexed by a small id. A side table maps id →
    (pc, vreg → {SSA value, constant, slot}). Once exits are cold stubs, the allocator stops
    seeing their uses as hot.
  - **Why:** code size and register pressure. Exits currently make every value live to the end
    of the region. This also enables B6 cold splitting.
  - **Deps:** none. It helps everything.

- [ ] <a id="t3"></a>**T3. Feedback recording (the "tier0-instrumented" equivalent)** —
  *Impact H, Size M*
  - **Ref:** `fgprofile.cpp` (block/edge count probes, class probes, method probes, and value
    probes for memmove sizes), `likelyclass.cpp` (likely-class tables), `fgIncorporateProfileData`,
    and `JitGuardedDevirtualizationChainLikelihood`, which cuts the guard chain at 75 %
    cumulative likelihood.
  - **lumen:** record the following in the interpreter:
    - (a) call-site target cells: last callee plus a polymorphic count, up to 4;
    - (b) per-conditional-jump taken/not-taken counters, saturating u16;
    - (c) per-arithmetic-site kind bits (int32 / f64 / string / other);
    - (d) polymorphic property ICs holding up to 4 shapes, in place of today's monomorphic
      `IcState`.

    Record only once a function is warm, as in .NET's tier0-instrumented stage, so cold code
    pays nothing.
  - **Lives in:** `bytecode.rs` (`Chunk` side tables) and `interpreter.rs` op handlers.
  - **Deps:** none.

- [ ] **T4. Synthesized profile when counts are missing** — *Impact M, Size S*
  - **Ref:** `fgprofilesynthesis.cpp`: static likelihoods such as "loop backedges are likely"
    and "throw paths are rare", propagated to block weights.
  - **lumen:** give `cfg.rs` block weights from the heuristics. The regalloc spill cost
    (`8^depth`) already approximates this. Use synthesized weights for layout, LICM and
    unrolling when T3 counts are absent: OSR-compiled regions compiled before the counters warm
    up, and AOT without training.
  - **Lives in:** `lumen-codegen/src/cfg.rs`, as a new `weights()` function.

- [ ] **T5. Static PGO for AOT (training run → blob)** — *Impact M (iOS/Puppeteer), Size M*
  - **Ref:** .NET static PGO: MIBC profiles are fed into crossgen/NativeAOT via
    `fgIncorporateProfileData`, and the "textual PGO" format is used for testing.
  - **lumen:** serialize the T3 tables (call targets, branch counts, IC shapes as property-key
    sequences rather than shape ids, since those are not stable across runs) into the
    `include_js!` precompiled blob next to the bytecode. The AOT compiler then speculates from
    them. Guards stay, because AOT code must still exit on a miss.
  - **Lives in:** `crates/lumen-aot`, `bytecode/serialize.rs`.
  - **Deps:** T3, and AOT native sections (`precompiled.rs`).

- [ ] **T6. Invalidation dependencies (lazy deopt)** — *Impact M, Size M*
  - **Ref:** none in RyuJIT; the nearest analogue is .NET's rejit on profiler request. JS needs
    its own design.
  - **lumen:** code that assumed a stable prototype chain, a global binding constant or a
    `Math` identity registers on a watchpoint. A mutation flips a flag that is checked at loop
    safepoints and on return. This removes per-iteration re-checks such as today's `MathGuard`
    re-check after any JS call.
  - **Deps:** T1, T2.

## J. JS tier (translator: `bytecode/jit/build.rs`, `layout.rs`, `helpers.rs`)

- [ ] <a id="j1"></a>**J1. Int32 representation with overflow/-0 guards** — *Impact H, Size M*
  - **Ref:** none directly: .NET integers are typed. The relevant machinery is IV widening
    (`optWidenIVs` in `inductionvariableopts.cpp`) and `rangecheck.cpp`, which proves no
    overflow.
  - **lumen:** add `Kind::Int32`, kept as I32 in SSA. Choose it from T3 kind bits or from the
    slot value at OSR time (an integral f64 in int32 range). `+ - *` use checked ops that exit
    on overflow. `*` also checks for a -0 result, and `/` and `%` check exactness. Compares,
    indexing and bit operations then need no conversion. When a merge sees Int32 on one edge and
    Num on the other, widen to Num.
  - **Why:** array indexing needs integer addresses (today `F64 → int` conversions sit on every
    `a[i]`). Loop counters get cheaper, and this unlocks G5, G6 and G7.
  - **Deps:** none, but it works best with T3.

- [ ] <a id="j2"></a>**J2. Inlining with a size-multiplier policy and a budget** —
  *Impact H, Size L*
  - **Ref:** `inlinepolicy.cpp`.
    - `DefaultPolicy::DetermineMultiplier` and `ExtendedDefaultPolicy` start from an additive
      multiplier. It gets bonuses for a constant argument that feeds a test, foldable branches,
      a call inside a loop, a hot call site and a struct/SIMD argument, and a penalty for a
      callee that contains loops.
    - A callee is accepted if its estimated size is at most the call-site size times the
      multiplier.
    - Callees below `ALWAYS_INLINE_SIZE` are always inlined.
    - The IL size cap is `DEFAULT_MAX_INLINE_SIZE` = 100, the depth cap is 20, and the compile
      time budget is `DEFAULT_INLINE_BUDGET` = 22× the root's estimate.
    - Observations are gathered by a cheap IL prescan (`fgFindJumpTargets` feeding
      `InlineObservation`s).
  - **lumen:** inline JS callees whose target is known (T3 call cell plus a callee guard, J3)
    when they are small: bytecode op count as the size proxy, bonuses for a constant or
    known-shape argument feeding a branch, for being inside a loop and for a hot site, and a
    penalty for loops. Use the same budget idea. A frame-state chain (inlined frames) lets
    deopt rebuild caller and callee interpreter frames. Getters and setters found by an IC
    are also candidates, and `this`-only accessors should always be inlined.
  - **Deps:** T1, T2, T3.

- [ ] <a id="j3"></a>**J3. Guarded devirtualization → callee/shape guards from feedback** —
  *Impact H, Size M*
  - **Ref:**
    - `indirectcalltransformer.cpp` (GDV expansion into guarded direct calls, chained for up to
      `MAX_GDV_TYPE_CHECKS` = 5 classes, cut at 75 % cumulative likelihood).
    - `fgResolveGDVs` / late devirtualization (after inlining exposes exact types).
    - `docs/design/coreclr/jit/GuardedDevirtualization.md`.
  - **lumen:**
    - Calls: `callee == F` followed by an inlined or direct call of F's compiled code; on a
      miss, a generic `Helper::Call`. If feedback says the site is monomorphic, deopt on the
      miss instead.
    - Properties: 2–4-shape inline dispatch, generalizing the monomorphic `layout.rs` loads.
    - Late devirtualization: after inlining, a receiver of known shape removes later guards.
      This comes from G2.
  - **Deps:** T3. Direct calls need T1.

- [ ] **J4. Keep numbers unboxed across merges, helper calls and exits (box elision)** —
  *Impact M-H, Size M*
  - **Ref:** box elision in the importer (`impImportBlockCode`, box/unbox.any pattern folding;
    `gtTryRemoveBoxUpstreamEffects`) and `fgOptimizeCast`.
  - **Partial:** today, when a block is entered from several edges with a non-empty operand
    stack, every stack entry is forced to Boxed.
  - **lumen:** merge stack entries through typed block parameters when all incoming kinds
    agree, so boxing happens only at exits. Add unboxed helper variants
    (`StoreLocalNum`-style) for numbers passed to helpers, and pass f64 arguments and returns
    unboxed to inlined or direct JS callees.
  - **Deps:** J1 for Int32. T1 for calls.

- [ ] <a id="j5"></a>**J5. Escape analysis + scalar replacement (object stack allocation,
  physical promotion)** — *Impact H on allocation-heavy code, Size L*
  - **Ref:**
    - `objectalloc.cpp`: a connection graph for escape. It stack-allocates classes, boxes and
      small arrays up to `JitObjectStackAllocationSize` = 528 bytes. Conditional escape clones
      the path where the object does not escape (`DeabstractionAndConditionalEscapeAnalysis.md`,
      which targets enumerators in `foreach`). Field tracking is also used.
    - `promotion.cpp` (physical promotion): replace struct fields with locals, weighted by the
      access counts.
  - **lumen:** for allocation sites in a compiled function (`MakeObject` with a pre-shaped
    `obj_maps` template, array literals, `arguments`, closures, and iterator result objects),
    an object whose uses are all known-shape loads and stores and which does not escape into
    helpers, the heap or returns becomes SSA values per property. Materialize it at deopt
    exits from the frame state; this is "rematerializable value" in jit.md. The
    iterator-protocol case (`for (x of array)`, where both the iterator and each `{value,
    done}` go away) mirrors .NET's enumerator de-abstraction.
  - **Deps:** T1, T2 (materialization at exits), J2. Iterator loops need build.rs support for
    iterator ops, which is currently unsupported.

- [ ] <a id="j6"></a>**J6. Refcount elision (the write-barrier analogue)** — *Impact H,
  Size M*
  - **Ref:** write-barrier elision (`gcIsWriteBarrierCandidate`, `optWriteBarrierAssertionProp`: skip the
    barrier for null/stack/known-young stores, and use a checked barrier only when needed) and
    `docs/design/coreclr/jit/GC-write-barriers.md`.
  - **lumen:** lumen uses `Rc` plus cycle collection, so the cost per Boxed move is
    `clone_value` / `drop_value` rather than a barrier. The borrowed-`Entry::Ref` scheme
    already avoids some clones (*partial*). Next steps:
    - (a) forward a clone to its matching drop and cancel the pair when nothing between them
      can run JS;
    - (b) move instead of cloning at the last use, reusing `jit_ir/liveness.rs`;
    - (c) skip refcount work for statically non-refcounted tags (tag ≤ 4) proven by guards;
    - (d) inline the fast path of increment and decrement without zero-crossing
      (count ± 1, compare, cold helper).
  - **Deps:** G2 for the facts. T1 improves it.

- [ ] **J7. Type-check and cast expansion (`typeof`, `instanceof`, `ToNumber`,
  `ToPropertyKey`)** — *Impact M, Size S*
  - **Ref:** `fgLateCastExpansion` in `helperexpansion.cpp`: profile-guided in-line checks for
    the likely class before a cast helper call (`JitProfileCasts`).
  - **lumen:** inline the tag test for `typeof x === "number"`-style patterns (they fold to a
    tag compare). For `instanceof`, guard on the IC'd prototype chain. Use the likely-kind
    feedback from T3 for `ToNumber` and `ToPropertyKey`.
  - **Deps:** T3 helps.

- [ ] **J8. More intrinsic expansion** — *Impact M, Size S each*
  - **Ref:** `importercalls.cpp` (`impIntrinsic`, named intrinsics) and
    `fgVNBasedIntrinsicExpansion`.
  - **Partial:** `Math.*` and `.length` are expanded today.
  - **lumen:** add `String.prototype.charCodeAt` / `charAt` / `[i]` on ASCII strings (the
    `LStr` ASCII bit is already in `layout.rs`), `Array.prototype.push` / `pop` on dense
    arrays, `Number.isInteger`, `Math.sign`, `Math.hypot`, `Array.isArray`,
    `String.fromCharCode`, and `Object.is`. Each gets an identity guard (cheaper with T6).

- [ ] **J9. try/finally and loops with handlers** — *Impact M (coverage), Size M*
  - **Ref:** `fgehopt.cpp`: `fgRemoveEmptyTry`, `fgRemoveEmptyFinally`, `fgCloneFinally`
    (copy the finally onto the normal-exit path), `fgMergeFinallyChains`, and
    `docs/design/coreclr/jit/finally-optimizations.md`. `eh-writethru.md` describes keeping EH
    live locals in registers and writing them through to the stack at every definition.
  - **lumen:** loops with `PushHandler` are currently rejected. Compile them by making the
    handler entry a deopt-style exit: on a throw, write back and resume the interpreter at the
    handler. Clone `finally` onto the normal path. EH write-through maps to the existing
    write-back-at-exit discipline.
  - **Deps:** T2.

- [ ] **J10. Switch recognition and bool folding** — *Impact L-M, Size S*
  - **Ref:** `switchrecognition.cpp` turns if-chains over one value into a switch (or bit-test);
    `optimizebools.cpp` merges `a && b` branch pairs into one compare.
  - **lumen:** `switch` over small int32 cases becomes a `BrTable` (the IR has it). A chain of
    `===` string compares against constants becomes a guarded string-identity table.
  - **Deps:** J1.

## G. IR-generic (`lumen-codegen`, target-independent)

- [ ] <a id="g1"></a>**G1. Memory-aware GVN / load CSE** — *Impact H, Size M*
  - **Ref:** `valuenum.cpp`: heap memory is split into per-field and per-array "memory
    states". A load gets the value number (VN) of `(memory state, address)`, calls kill memory,
    and stores update only their own alias class. See also "Optimization of Heap Access in
    Value Numbering.md" and `optcse.cpp`.
  - **lumen:** give `Load` and `Store` an alias class, a small integer set by the front end:
    frame slot *s*, stack entry *d*, object header, props-entries, element storage, and
    "unknown". GVN keeps a memory version per class, and calls and `Store`s bump the version.
    Loads are then CSE-able.
  - **Why:** repeated shape-id, entries-pointer and length loads are the bulk of guard cost.
  - **Lives in:** `ir.rs` (`MemKind` gains a class), `opt.rs` (`gvn`).

- [ ] <a id="g2"></a>**G2. Assertion propagation / redundant guard and branch elimination** —
  *Impact H, Size M*
  - **Ref:**
    - `assertionprop.cpp`: facts generated by compares, null checks, type checks and bounds
      checks, propagated along dominators and edges.
    - `redundantbranchopts.cpp`: `optRedundantBranch` / `optRedundantDominatingBranch`
      infer a branch's outcome from a dominating compare, including implied relations via
      `optRelopImpliesRelop`, and `optRedundantRelop`.
    - `earlyprop.cpp`: null-check folding and array-length propagation.
  - **lumen:** walk the dominator tree with a fact table of the form `value has tag T`,
    `object has shape S`, `x < len`, and `x ∈ int32`. A guard or `brif` whose outcome is
    implied is removed. The translator's per-region "caches" (reset after JS) become a
    special case once helpers carry "may run JS" effect bits.
  - **Deps:** G1 for load equality.

- [ ] **G3. Loop canonicalization: inversion (rotation), preheaders, single exit blocks** —
  *Impact M, Size S*
  - **Ref:** `optInvertLoops` (while → `if (c) do {} while (c)`), `optCanonicalizeLoops`
    (preheaders, exits), and `FlowGraphNaturalLoop` in `flowgraph.cpp`.
  - **Partial:** `cfg.rs` computes loop depth, and the wasm-side translator has rotation.
  - **lumen:** rotate `for` loops so the test sits at the bottom (one branch per iteration)
    and add a dedicated preheader block. This is needed by G4, G6 and G7.

- [ ] <a id="g4"></a>**G4. Loop-invariant code motion (LICM)** — *Impact H, Size M*
  - **Ref:** `optHoistLoopCode` in `optimizer.cpp` hoists VN-invariant expressions to the
    preheader. It is gated by register pressure (it counts live and hoisted values against the
    callee-saved budget) and by a cost check for expressions that are not always executed.
  - **lumen:** hoist pure invariants plus *guards* and G1 loads whose memory class is not
    written in the loop (shape checks on loop-invariant receivers, array element-storage base
    and length when the loop does not store to the array header). A hoisted guard exits at the
    loop entry state, so it needs T2 frame states for the preheader.
  - **Deps:** G1, G3, T2.

- [ ] <a id="g5"></a>**G5. Range analysis and bounds-check elimination** — *Impact H,
  Size M*
  - **Ref:**
    - `rangecheck.cpp` computes symbolic `[lo, hi]` ranges over SSA and phis with overflow
      checks, and removes checks proven in range.
    - `boundscheckcoalesce.cpp` (`optBoundsCheckCoalesce`) merges `a[i+1]`, `a[i+3]` checks
      into one check against the maximum offset.
    - `rangecheckcloning.cpp` (`optRangeCheckCloning`) clones a block of several checks
      against the same (index, length) into a fast path behind one combined check.
  - **Partial:** the translator's `i < a.length` fact.
  - **lumen:** range analysis over Int32 (J1) SSA values, with the loop condition and G2
    facts as sources. Handle `a[i]`, `a[i-1]`, `a[i+k]`, reverse loops, and nested
    `a[i][j]`. Coalesce constant-offset groups.
  - **Deps:** J1, G2.

- [ ] <a id="g6"></a>**G6. Induction-variable opts: widening, strength reduction, downward
  counting, dead IV removal** — *Impact M, Size M*
  - **Ref:** `inductionvariableopts.cpp` with `scev.cpp` (add-recurrences):
    - `optWidenIVs`: i32 → i64 IVs to drop the sign extension in addressing (x64);
    - `StrengthReductionContext::TryStrengthReduce`: replace `base + i*16` with a pointer that
      advances by 16;
    - `optMakeLoopDownwardsCounted`: count to 0 to save the compare;
    - `optRemoveUnusedIVs`.
  - **lumen:** element addressing `elems + i*16` (the `Value` size) or `i*8` (the f64 mirror)
    becomes a pointer IV once BCE has removed the checks. On AArch64 the scaled index folds
    into the address mode, so widening matters only on x64.
  - **Deps:** J1, G3, G5.

- [ ] <a id="g7"></a>**G7. Loop unrolling** — *Impact L alone, M with inlining + Int32;
  Size S (full) / M (partial)*
  - **Ref:** `optUnrollLoops` / `optTryUnrollLoop` in `optimizer.cpp`.
    - It handles **full unrolling only**: constant init and constant limit, recognized by
      `AnalyzeIteration`. The trip count comes from `optComputeLoopRep`.
    - The default maximum is 4 iterations (`DEFAULT_UNROLL_LOOP_MAX_ITERATION_COUNT`,
      `compiler.h`), or more when the limit is `Vector<T>.Count`. Hard caps are 10 iterations
      (blended) and 20 (FAST_CODE).
    - Code growth `(trip-1)*body + 8` must stay ≤ 300 (blended) or 600 (fast) cost units.
    - Loops with 0 or 1 trips are always unrolled.
    - It skips cold loops and loops with EH.
    - It processes inner loops first and repeats up to 10 passes.
    - It runs **before** SSA, so the copies are cleaned up by later VN, constant propagation
      and CSE. That cleanup is where the benefit comes from.
    - There is no partial unrolling in RyuJIT.
  - **lumen, full unroll:** a loop `for (let i = C0; i <op> C1; i += C2)` becomes a straight
    sequence of body copies with `i` a constant in each, when all of the following hold:
    - `i` is an Int32 or Num local that is only written by the update;
    - `i` is not captured, so no per-iteration binding is needed. A captured `i` needs a
      fresh environment per copy, so skip it;
    - the trip count is ≤ 4 (≤ 8 when the body is pure arithmetic), and
      `(trip-1) * body_ops ≤` about 300 IR instructions;
    - the loop has no handler.

    Clone the IR blocks of the loop, substitute the constant IV, and let GVN fold. The
    backedge and its safepoint-budget decrement disappear.
  - **Is it worth it for `for (let i = 0; i < 3; i++) call();`?** Only after inlining:
    - Without inlining, unrolling saves the compare, increment, branch and budget decrement:
      about 4–6 instructions per iteration against a JS call through `Helper::Call` that costs
      hundreds of instructions. That is under 2 %, so don't bother.
    - With `call` inlined (J2), each copy sees a constant `i`. Arguments fold, `a[i]` with a
      constant index needs one combined check (G5 coalescing), and branches on `i` fold. That
      is where the win is.
    - Put unrolling after inlining and before G1/G2 in the pass order (as RyuJIT does before
      VN).
  - **lumen, partial unroll:** unroll by 2–4 with a remainder loop for tight numeric loops
    with a non-constant trip count. It is not in RyuJIT; LLVM does it. The benefit in a
    speculative JIT is small: guards and exit states multiply, and out-of-order cores already
    overlap the loop overhead. Defer until G5 and G6 exist, then measure. Partial unrolling
    is most useful on wasm32, where the browser JIT (not lumen) sees each branch; even there,
    only by 2.
  - **Lives in:** a new `lumen-codegen/src/loops.rs`, with a block-cloning utility shared
    with G8 and B5.
  - **Deps:** G3, J1. The value needs J2.

- [ ] <a id="g8"></a>**G8. Loop cloning (versioning)** — *Impact M, Size M*
  - **Ref:** `loopcloning.cpp` (`optCloneLoops`). It clones a loop into a fast copy (bounds
    checks removed, type tests hoisted — `JitCloneLoopsWithGdvTests`) and a slow copy, chosen
    by conditions checked once in the preheader. It is limited by `JitCloneLoopsSizeLimit` =
    400 and a per-call ratio.
  - **lumen:** when a guard cannot be hoisted because it may fail midway (for example, an
    array that may be resized in a helper), version the loop instead: fast copy on
    `len ≥ n && shape == S`, slow copy with guards. Otherwise LICM with deopt (G4) is cheaper
    and simpler in JS, so do this only for loops with frequent deopts.
  - **Deps:** G4, G5, T3.

- [ ] **G9. Forward substitution and a pressure-aware CSE policy** — *Impact M, Size S*
  - **Ref:**
    - `forwardsub.cpp` (`fgForwardSub`) substitutes single-use temps into their user so tree
      patterns match.
    - `optcse.cpp` does not CSE everything. A candidate must have cost ≥ 2, there are at most
      64 candidates, and each is promoted to a register only if its use weight clears a
      threshold derived from frame pressure. The threshold is higher when the value is live
      across a call.
    - Newer versions add a reinforcement-learning CSE policy (`JitRLCSE*`, a parameterized
      policy trained offline). It is overkill for lumen.
  - **Partial:** lumen's GVN CSEs every pure duplicate, including cheap ones across calls.
    That causes spills, so the cost is register pressure rather than extra work.
  - **lumen:** skip CSE for cheap ops (constants are already rematerialized, and one-op
    arithmetic belongs here too) whose live range would span a call. Undo CSE by
    rematerialization in regalloc instead of spilling.

- [ ] **G10. If-conversion** — *Impact L-M, Size S*
  - **Ref:** `ifconversion.cpp` (`optIfConversion`) turns a small diamond or triangle with a
    single store into `select`/cmov. Each arm's cost limit is small (about 7), and loops are
    skipped unless the branch is unpredictable (per the profile).
  - **lumen:** `Math.min`/`max`-style patterns, `x = c ? a : b` on unboxed kinds, and
    `abs`/`clamp`. The IR has `Select`, and both backends lower it to cmov/csel.
  - **Deps:** T3 branch counts to avoid converting predictable branches.

- [ ] **G11. Head/tail merge and tail duplication** — *Impact L, Size S*
  - **Ref:** `fgHeadTailMerge` (merge identical block tails, and identical heads of
    successors) and `fgOptimizeUncondBranchToSimpleCond` (duplicate a small conditional block
    into a predecessor's jump — tail duplication).
  - **lumen:** tail-merge identical exit or deopt stubs. Mostly subsumed by T2.
  - **Deps:** T2.

- [ ] **G12. VN-based dead-store removal** — *Impact L-M, Size S*
  - **Ref:** `optVNBasedDeadStoreRemoval` removes a store whose value equals the value already
    in memory (same VN).
  - **lumen:** write-backs of unchanged SSA locals at exits, and `slot = slot` stores. With
    T2 the exit write-back becomes metadata, so this matters for in-loop `frame.slots`
    stores of Boxed values.
  - **Deps:** G1.

## B. Backend (lowering, regalloc, emit; per target)

- [ ] **B1. More containment: load-op, RMW, scaled index, LEA** — *Impact M, Size M*
  - **Ref:** `lowerxarch.cpp`: `ContainCheckBinary` (memory operand of an ALU op),
    `LowerStoreIndir` / `IsRMWMemOpRootedAtStoreInd` (`add [mem], reg`), and
    `CreateAddrMode` in `lower.cpp` (`[base + index*scale + disp]`). `lowerarmarch.cpp`
    covers madd/msub, shifted operands and `cmp` with a shifted register.
  - **Partial:** immediates, compare-branch fusion, and `iadd` into `[base+disp]` or
    `[base+index]`.
  - **lumen:**
    - x64: fold `ishl i, 4` plus `iadd` into a scaled index (`[base+i*8]`; a 16-byte `Value`
      needs `*16`, so shift once and use `*8`/`*2`); contain single-use loads in
      `add`/`cmp`/`ucomisd`; use RMW for the safepoint budget decrement
      (`sub qword [frame+budget], 1`).
    - AArch64: `ldr x, [base, i, lsl #3]`, `madd`, and `cmp w, w, lsl`.

- [ ] **B2. Register allocation: live-range splitting at block boundaries and around calls** —
  *Impact M-H, Size M*
  - **Ref:** `lsra.cpp`: interval splitting at block boundaries with resolution moves,
    spill-at-def (single-def) placement, `RefTypeKill` handling of calls, register hints
    (`registerAssignment` preferences, copy-reg), and the heuristics in
    `docs/design/coreclr/jit/lsra-heuristic-tuning.md`.
  - **Partial:** lumen has one location per value for its whole lifetime, so any value that
    lives across a single helper call in a cold path is spilled everywhere.
  - **lumen:** split around calls that sit in cold blocks (reload after), prefer callee-saved
    registers for values live across hot calls, and coalesce block-parameter copies via hints
    on the successor's parameters.
  - **Deps:** B6 weights help.

- [ ] **B3. Emitter peepholes** — *Impact L-M, Size S*
  - **Ref:**
    - `emitxarch.cpp`: `IsRedundantMov` (drop `mov r, r`, and a mov that repeats the last one
      in reverse) and `AreFlagsSetToZeroCmp` / `IsRedundantCmp` (skip `test r, r` when the
      previous ALU op set the flags).
    - `emitarm64.cpp`: `IsRedundantLdStr` (drop a load that follows a store to the same
      slot) and `ReplaceLdrStrWithPairInstr` (merge adjacent `ldr`/`str` into `ldp`/`stp`).
  - **lumen:** all of these apply directly to `x64/emit.rs` and `aarch64/emit.rs`. `ldp`/`stp`
    especially suits 16-byte `Value` copies (tag word plus payload) in exits and spills.

- [ ] **B4. Small fixed-size copy unrolling** — *Impact L-M, Size S*
  - **Ref:** `lower.cpp` (`LowerBlockStore`, `getUnrollThreshold` in `compiler.h`):
    - memmove, memset and memcmp of constant size are unrolled up to about one to four vector
      registers (16/32/64 bytes, depending on ISA), with larger sizes on ARM64 via `ldp`/`stp`
      pairs;
    - `importervectorization.cpp` vectorizes string equality and `StartsWith` against
      constants.
  - **lumen:**
    - 16-byte `Value` moves become one `movups` or one `ldp`/`stp`. Today there are two 8-byte
      moves.
    - Materializing N stack entries becomes an unrolled vector copy.
    - Comparing strings to constants (`s === "div"`) becomes a length check plus one or two
      wide compares, since `LStr` bytes are UTF-8 with an ASCII bit.
    - On wasm, these map to `v128.load`/`v128.store` once SIMD is enabled.

- [ ] **B5. Loop alignment** — *Impact L, Size S*
  - **Ref:** `placeLoopAlignInstructions`: align hot inner-loop heads to 32 bytes when the
    loop is ≤ 96 bytes (`DEFAULT_ALIGN_LOOP_BOUNDARY`, `DEFAULT_MAX_LOOPSIZE_FOR_ALIGN`),
    adaptive padding, and padding hidden behind an unconditional jump.
  - **lumen:** x64 only; Apple cores are less sensitive. Mostly reduces benchmark noise.

- [ ] <a id="b6"></a>**B6. Block layout with profile weights + cold placement / hot-cold
  split** — *Impact M, Size M*
  - **Ref:**
    - `fgSearchImprovedLayout` / `ThreeOptLayout` in `fgopt.cpp`: start from an RPO layout,
      then run greedy 3-opt with cost "edge weight not falling through", keeping loop bodies
      contiguous.
    - `fgDetermineFirstColdBlock` plus `hot-cold-splitting.md`: move cold blocks into a
      separate code region.
  - **lumen:**
    - Layout is currently translator order. Use RPO with cold blocks last: exits, deopt stubs,
      helper slow paths and throws.
    - Once T3 branch counts exist, run 3-opt on hot blocks.
    - Splitting into a separate region is not needed; putting cold code at the end of the
      function gets most of the i-cache benefit.
    - wasm is unaffected: layout is decided by the structured translation and the browser's
      compiler.
  - **Deps:** T4 or T3.

- [ ] **B7. Frame and prologue trimming** — *Impact L, Size S*
  - **Ref:** `codegencommon.cpp` (`genFnProlog`: save only the callee-saved registers
    actually used, no frame pointer in leaf funclets), stack probes only for large frames
    (`genAllocLclFrame`), and `arm64-jit-frame-layout.md`.
  - **lumen:** matters once T1 makes many small functions (per-call entry cost). Don't save
    unused callee-saved registers, and use `stp` pairs.
  - **Deps:** T1.

- [ ] **B8. wasm32: local reuse and fewer locals** — *Impact L-M (browser compile time and
  size), Size S*
  - **Ref:** RyuJIT now has a wasm target: `regallocwasm.cpp` (allocate wasm locals rather
    than one per temp), `fgWasmControlFlow` / `fgWasmTransformSccs` (reducible structured
    flow; lumen already has the Ramsey equivalent), and `fgWasmSpillRefs`.
  - **lumen:** `wasm/func.rs` currently gives each SSA value its own local. Run the existing
    linear scan with an unbounded register file per type to assign shared locals. The browser
    engine's allocator then works with far fewer locals, and modules get smaller.
  - **Deps:** none.

## Things from RyuJIT to skip (or already covered)

- **Struct ABI, multi-reg returns, first-class structs, implicit by-refs.** JS has no value
  types. The useful part, promotion, is covered by J5.
- **Stack-probe and GS-cookie phases, TLS/static-init expansion.** For TLS and static init,
  the JS analogue (global binding caches) is already in `NamePtr`/`CapPtr` and gets better
  with T6.
- **Tier0 as a compiled tier.** lumen's interpreter plays tier 0 plus instrumented tier 0.
  A baseline template JIT was removed on purpose (see `optimizer.md`). Don't bring it back.
- **Custom calling conventions.** Out of scope: wasm32 cannot use them, and direct JS→JS
  calls (T1) should use one internal convention (frame pointer plus unboxed-argument area)
  that works on all three targets.
- **The RL CSE heuristic, ML inline policies** (`JitInlinePolicyModel`, profile model). These
  are research tooling; use the simple additive policy from J2.

## Suggested order of work

1. T2 → T3 → J1: foundations with standalone wins.
2. G1 → G2 → G3 → G4 → G5: loop-tier speedups on today's regions.
3. T1 → J3 → J2 → J4: calls. This is the biggest step toward DeltaBlue/Djot parity.
4. J6 → J5: allocation and refcount traffic.
5. G7 → G6 → G8, then the B items as measured: polish.

Each step lands behind the existing gates: test262 unchanged with `LUMEN_JIT_EAGER=1`, the
differential suite, and a V8-v7/Djot before-and-after comparison (see jit.md).
