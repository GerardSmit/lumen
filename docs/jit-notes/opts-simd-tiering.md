# RyuJIT: mid-level optimizations, SIMD, tiering and Satori barrier notes

Paths are relative to Satori: J = `src/coreclr/jit`, VM = `src/coreclr/vm`,
GC = `src/coreclr/gc/satori`.

## Pass order
The real order is the `DoPhase` sequence in J`compiler.cpp:4309-5080`. Do not go by
`compphases.h`, which only declares the phases.

**Before SSA**
1. find loops
2. loop inversion
3. block weights
4. loop cloning
5. unroll
6. dominators

**SSA-based**
1. SSA
2. EarlyProp
3. VN (value numbering)
4. hoist
5. copy prop
6. redundant branches
7. CSE (common subexpression elimination)
8. bounds-check coalesce
9. assertion prop
10. range check
11. IV opts
12. VN dead-store removal
13. range-check cloning

**Backend**
1. if-conversion
2. lowering
3. LSRA
4. 3-opt block layout (after register allocation)
5. loop alignment

### Recommended lumen pipeline
Ordered by payoff:
1. Fold constants while building the IR, and forward-substitute temps that have a single use.
2. Inline. The model is: inline when the callee's size is at most the call site's size times a
   multiplier. The multiplier is additive:
   - +3 for a foldable branch or a constant argument that feeds a test
   - +2 for a warm call site, +3 for a hot one
   - ×0.7 when the callee contains loops

   The time budget is linear, about 22× the root function's cost estimate. For JS, add bonuses
   for a monomorphic IC site, a known-shape argument, and small accessors.
3. CFG cleanup, then **loop inversion** (while → guarded do-while), then preheaders.
4. **GVN with a memory model.** Memory versions per alias class with a lookup budget. This one
   pass stands in for VN + CSE + copy prop.
5. **Redundant-branch and type-guard elimination.** Walk the dominator tree and apply implied
   relations (`optRelopImpliesRelop`). Thread jumps through block parameters. This is high value
   for JS.
6. **LICM with a register-pressure gate** (optimizer.cpp:4055). Hoist only while callee-saved
   registers remain for hoisted values. Hoist a value that is not computed on every iteration
   only if it costs at least 16× a load.
7. Bounds-check elimination via range analysis, IV strength reduction, and i32→i64 IV widening
   (x64 only; ARM64 folds the sign extension into the address).
8. Loop versioning gated on profile data (size limit about 400).
9. If-conversion outside loops, only when each arm costs ≤ 7.
10. Lowering and containment (see [lowering-emit.md](lowering-emit.md)).
11. LSRA (see [lsra.md](lsra.md)).
12. Block layout: loop-aware RPO over hot blocks, then greedy 3-opt with
    cost = weight − fall-through weight, cold blocks last.
13. Emit: loop alignment and a constant pool.

### CSE promotion model (optcse.cpp:4351)
- Thresholds come from frame pressure: the weight of about the 13th local ("aggressive") and
  the 39th ("moderate").
- A candidate must have cost ≥ 2, and there are at most 64 candidates.
- Def/use costs rise under pressure: aggressive 1/1, moderate 2/1, conservative 2/2 or 2/3.
  They also rise when the value lives across a call.

## SIMD (for wasm v128 later)
- IR: a single `HWIntrinsic` node kind, with value type and lane type kept separate
  (gentree.h:6711). `VecCon` holds 16 bytes of vector constant.
- **Table-driven ops.** Each row of an X-macro table is
  `(isa, name, size, nargs, instruction per lane type [10], cost, category, flags)`.
  - Examples of flags: `Commutative`, `RMW`, `ReturnsPerElementMask`, `SpecialLowering`,
    `NoContainment`.
  - Lowering and codegen are generic for table-driven rows (`genHWIntrinsic_R_R_RM`).
  - Special rows go through a `SPECIAL` escape.
- **A wasm table already exists**: J`hwintrinsiclistwasm.h` (the `PackedSimd` ISA). Use it to
  seed the op/lane matrix for a wasm-SIMD table in Rust.
- **Ops that need special lowering:**

  | Op | x64 | ARM64 |
  |---|---|---|
  | `i8x16.shuffle` | pshufd/shufps/palignr patterns, else `pshufb` with a constant | `TBL` |
  | `swizzle` | `paddusb idx, 0x70` + `pshufb` | `TBL` |
  | `f32x4.min/max` | NaN/-0 fixup (gentree.cpp:26450) | |
  | `trunc_sat` | fixup | `FCVTZS` already saturates |
  | `i64x2.mul` | no SSE instruction | |
  | `i8x16.shl/shr` | widen, shift, narrow | |
  | `popcnt` | `pshufb` nibble lookup table | `CNT` |
  | `bitmask` | `pmovmskb` | `ushr`/`addv` |

- **Constants.** Zero is `xorps`, all-ones is `pcmpeqd`, anything else comes from the constant
  pool. Compress a constant with repeated halves into a broadcast load.
- **ISA baseline:** SSE4.1. Use AVX (VEX three-operand forms) when present; this removes the
  RMW constraint.

## Tiering / OSR / PGO → JS
- The .NET baseline is 30 calls plus a 100 ms delay that re-arms while startup continues, then
  a background compile. The interpreter is our instrumented tier.
- **OSR.**
  - Keep one counter per frame (starting at 1000), decremented at each loop head. When it hits
    zero, call a helper.
  - The helper reloads the counter with 1000 and compiles synchronously after 10 hits.
  - A failed compile marks the patchpoint invalid.
  - For lumen: compile the OSR entry as a separate function whose entry block receives the
    interpreter's live registers as block parameters. This is simpler than .NET's in-place
    frame reuse.
- **Profiles.**
  - Branch counts per conditional bytecode are enough; no spanning-tree edge instrumentation.
  - IC shape lists act as the class profile. Build guard chains that cover ≥ 75% of observed
    shapes cumulatively, adding any shape seen ≥ 10% of the time.
  - Prefer a guarded fast path plus a slow path. Deopt only where the guard must be exact.

## Satori GC and codegen
- **The JIT is unchanged under Satori.** The differences live in the helpers.
- **Barrier (patchedcode.asm:203):**
  1. Is the destination in the heap? Check a 1GB-granular byte map.
  2. A null source goes straight to the assign.
  3. If the source's 2MB region is owned by this thread and the destination is in the same
     region, it is a plain store, unless the slot's "exposed" bit is set. Otherwise call the
     escape function.
  4. Card marking is skipped for same-region stores and gen2 sources. It uses a three-level
     hierarchy: card, group, page.
- **Allocation is a helper call** doing a thread-local bump (two loads, a compare, a store).
  Allocation contexts are pre-zeroed, so the JIT skips zeroing new objects.
- **Lesson for lumen:**
  - Inline a 3–5 instruction barrier filter and put the rest out of line.
  - Inline the bump allocation.
  - Use a back-edge poll (`cmp [flag],0; jne slow`) that also serves as the OSR/interrupt
    check, instead of fully interruptible GC maps.
