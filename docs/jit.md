# Optimizing tier roadmap

## Status

Milestone 1 is partly in: the loop tier in `crates/lumen/src/bytecode/jit/` (contract in its
`mod.rs`). A hot loop of a bytecode chunk (1024 backedges; `LUMEN_JIT_EAGER` compiles at the
first one) is translated to `lumen-codegen` IR, optimized, and entered by OSR at its header.
Locals holding numbers or booleans at entry live unboxed in SSA behind entry guards; anything
else stays in the frame and goes through helpers, with a single-op interpreter fallback
(`Helper::Generic`) for ops without a native path. Failed speculation, region exits and throws
return an exit word, and the interpreter resumes at that pc with the materialized stack.

Backends: x86-64 and AArch64 machine code (AAPCS64 and Apple ABIs; not on iOS, which forbids
runtime code generation), and on wasm32 a WebAssembly module over the engine's own memory and
function table, instantiated by the embedder (`lumen::set_wasm_jit_host`; `lumen-wasm` does it
with the synchronous `WebAssembly.Module`, which browsers may limit on the main thread — run
lumen in a Worker).

Verification: test262 passes unchanged with `LUMEN_JIT_EAGER=1`; a differential suite compares
JIT, wasm32 and Node output. Env flags: `LUMEN_NO_JIT`, `LUMEN_TIER_LOG`, `LUMEN_JIT_DUMP=<dir>`.

Not yet: whole-function compilation, inlining, invalidation dependencies, loops with handlers
or iterator ops, and ahead-of-time native sections (see `crates/lumen/src/precompiled.rs`).

The current JIT is a per-op template compiler over the stack bytecode. It never deoptimizes: a
function compiles whole or not at all, and every op carries its generic fallback in line. Speed
has come from pattern-matched "regions" (numeric chains, loop plans, name paths, inline
frames), each a special case with its own guards and exits. The ARM64 backend has almost all of
them; x86-64 mostly calls helpers. After the V8-v7 gap stopped closing (about 10x behind Node,
40x on DeltaBlue and EarleyBoyer), that design has run out of room: each new region gains 1-5%,
and none of the facts one region proves survive to the next op.

The replacement is a general optimizing tier: an SSA IR built from bytecode plus type
feedback, speculative guards that bail out to the interpreter frame, a small set of classic
optimizations, and one backend shared by ARM64 and x86-64 from a common lowering.

## What we take from RyuJIT (.NET)

- **Pipeline shape**: build CFG + IR, inline, fold, SSA + value numbering, redundancy
  elimination (CSE, redundant branches, assertion/range propagation), loop-invariant
  hoisting, lowering with operand containment, linear-scan register allocation on the lowered
  IR, per-target codegen, table-driven emitter. `src/coreclr/jit/compiler.cpp`
  (`compCompile`) is the reference for which phases matter; most are optional under MinOpts.
- **Tiering with PGO**: the baseline tier only instruments functions once they are warm, and
  the optimizing tier uses the profile for block weights, layout and hot/cold splitting.
- **One-way OSR at loop headers**: the baseline keeps every local in a fixed frame slot at loop
  headers, so a single descriptor per function (vreg to slot) lets optimized code take over a
  running frame without per-point maps.
- **Two targets, one IR**: target differences enter at lowering (legal immediates, address
  modes, read-modify-write forms) and in the register constraints the allocator sees; codegen
  is one switch per target and the emitter is target-specific.
- **Linear scan that compiles fast**: one pass over RefPositions, temps single-def/single-use,
  splitting only at block boundaries and spills, edge resolution moves.

## What must come from JS engines instead

.NET needs no deoptimization because its types are static: a failed guarded devirtualization
falls back to a real virtual call whose result type is still known, so code after the merge
stays typed. In JS the fallback of `o.x` can return anything, every later op would need its
own guard, and the facts would die at every merge. That is exactly the lumen JIT's current
problem. So:

- **Speculative guards** (`CheckShape`, `CheckInt32`, `CheckNumber`, `CheckBounds`,
  `CheckNotHole`, overflow checks) branch to a shared **deopt exit**, not an in-line fallback.
  After a guard its facts hold for the rest of the function.
- **Frame states**: each guard records how to rebuild the interpreter frame (vreg to SSA value,
  constant, or rematerializable value) and the bytecode pc to resume at.
- **Bailout**: box values, rebuild the frame, resume in the bytecode VM. A function that keeps
  bailing is invalidated and recompiled with widened feedback.
- **Dependencies**: shape transitions and prototype mutations invalidate code that assumed
  them (lazy deopt at return addresses and safepoints).
- **Polymorphism**: where feedback shows 2-4 shapes, dispatch them in line (RyuJIT-style guarded
  chain) and fall to the generic IC; megamorphic sites call the IC directly.

## IR

A flat SSA IR (blocks of instruction lists with explicit phis) rather than RyuJIT's HIR trees,
which exist to digest an IL stack machine. The same IR is lowered to machine-level ops before
register allocation. Values are 8-byte NaN-boxed words (see `docs/gc.md`); unboxed int32 and
f64 exist only between a guard and a box.

## Milestones

1. **Core**: bytecode to SSA with feedback guards and frame states for leaf functions and
   single loops: int32/f64 arithmetic, locals, monomorphic property get/set, array element
   access. GVN plus dominator-ordered redundant-guard elimination. Lowering, linear scan,
   x86-64 and ARM64 emitters. Deopt to the interpreter; OSR entry at loop headers. Behind a
   flag, with a stress mode that forces a bailout at every guard.
2. **Calls**: inlining of monomorphic targets, polymorphic in-line dispatch, invalidation
   dependencies.
3. **Loops**: invariant hoisting of shape and bounds checks, range analysis, bounds-check
   elimination.
4. **Retire regions**: remove the template JIT's pattern regions as the optimizing tier covers
   them; the template JIT stays as the baseline tier (and grows the feedback it records).

Each milestone lands with test262 unchanged on every tier, the differential fuzzer extended to
the new tier, and a V8-v7 / Djot comparison against the previous milestone.

## No benchmark-specific code

Optimizations are keyed on guards over shapes, types and feedback, never on property names,
function names or the exact structure of a known program. The Richards scheduler
specialization that previously lived in `jit.rs` was removed for this reason.
