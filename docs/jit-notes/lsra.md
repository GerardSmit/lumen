# RyuJIT LSRA lessons (from Satori's src/coreclr/jit)

Source study for `lumen-codegen`'s register allocator. Paths are relative to `src/coreclr/jit`.
RyuJIT allocates non-SSA locals (one interval with many defs). Under SSA with block parameters,
several of its mechanisms become simpler or unnecessary.

## Copy
1. **Two locations per instruction.** Uses sit at `2i` and defs at `2i+1`. A *delay-free* use
   stays busy through the def location, which covers non-commutative RMW op2 on x64
   (`sub d, b`: b must differ from d) and the divisor of `div`. An instruction's scratch
   registers are *internal intervals* at the use location (lsrabuild.cpp:1299).
2. **Fixed-register and kill RefPositions** on physical registers, plus the per-register arrays
   `nextFixedRef[reg]` and `nextIntervalRef[reg]`. Covers, best-fit and far-next-ref are then
   O(registers) with no interval-tree queries (lsra.cpp:272-419).
3. **Selectors as an ordered filter over a bitmask.** Each selector keeps its intersection
   with the candidates only if that is non-empty, and selection stops at one register
   (lsra_score.h, lsra.cpp:12603).
   - Free registers: previous register (shortcut) → covers ∩ preference → preference →
     related-covers → caller/callee choice → covers-full → best-fit → register order.
     Best-fit means: when some register covers the interval, take the one killed soonest;
     otherwise take the one free longest.
   - Busy registers: lowest spill cost → farthest next reference (Belady) → register number.
   - A register-optional use does not evict anything cheaper than itself; it is used from
     memory instead (lsra.cpp:13103).
   - A register already holding the same constant is reused (CONST_AVAILABLE).
4. **Weights.** `BB_UNITY = 100`, multiplied by 8 per loop nesting level. A loop block that does
   not dominate the back-edge source gets ×4 instead (optimizer.cpp:173). A value's spill cost is
   the sum of its references' block weights. Temporaries get a ×4 boost; a value that is already
   spilled is discounted (lsra.cpp:174).
5. **Spill at the def.** Under SSA every value has a single def (RyuJIT's `singleDefSpill`,
   lsra.cpp:3461). The stack slot stays valid for the value's whole life, so no
   register-to-stack move is ever needed on an edge. Reloads happen at uses.
6. **Preferencing.**
   - Related intervals: link each block argument to its block parameter, and an RMW op1 or
     shift source to the def when it dies there.
   - Merge fixed-use masks (argument registers, return register) into earlier preferences.
   - A value live across a call gets `preferCalleeSave` and an aversion to killed registers.
   - Floating-point values only need to cover the current reference (lsra.cpp:13490), which
     keeps them out of callee-saved registers.
7. **Block order and entry state.** Allocate in loop-aware RPO. Seed each block's entry state
   from its highest-weight predecessor that is already allocated, so mismatches land on cold
   edges. For a latch whose sibling target was already visited, reuse that sibling's
   predecessor (the back-edge trick, lsra.cpp:2400).
8. **Edge resolution.**
   - A split edge puts its moves at the top of the successor; a join edge puts them at the
     bottom of the predecessor.
   - On a critical edge, a move that is the same for every successor goes once at the end of
     the source block; any other move splits the edge. Registers read by the terminator are
     excluded.
   - Parallel moves: register-to-stack moves first, then register-to-register moves with a
     ready set, then stack-to-register moves. Break cycles with a free caller-saved register;
     on x64 integer cycles use `xchg`; as a last resort go through the stack (lsra.cpp:9463).
9. **Rematerialize constants on spill.** RyuJIT does not do this, but it is cheap in SSA.
10. **x64 specifics.**
    - Shifts: BMI2 `shlx`/`sarx`/`shrx` avoid the RCX constraint.
    - Division: op1 is fixed to RAX, the result is RAX (quotient) or RDX (remainder), and the
      divisor is delay-free and excludes RAX and RDX.
    - Commutative operations with an op2 in a register swap operands instead of needing
      delay-free.
11. **Register order: caller-saved first.** This is the only prolog-cost model: callee-saved
    registers are chosen only when a value is live across a call.
    - x64 Windows ints: RAX RCX RDX R8 R10 R9 R11 | RBX RSI RDI R14 R15 R13 R12.
    - x64 Windows floats: XMM0-5 | XMM6-15. Callee-saved are RBX RSI RDI R12-R15 (and RBP),
      plus XMM6-15.
    - x64 SysV: callee-saved are RBX and R12-R15; no floats are callee-saved.
    - ARM64 ints: R0-R15 | R19-R28. ARM64 floats: V16-V31 first, then V0-V7, then V8-V15.
      Only the low 64 bits of V8-V15 are callee-saved.
12. **Parameters.** A cold parameter (weight ≤ UNITY) is not given a register at entry.

## Skip
EH write-thru, partial SIMD callee-save, ARM32 register pairs, ARM64 consecutive registers,
stress modes, full per-block `VarToRegMap` DummyDef/ExpUse (SSA liveness per edge suffices), and
the complexity of `resolveConflictingDefAndUse` (insert an explicit copy instead).

For "extend to the loop end": treat the latch's block arguments and loop-carried live-outs as uses
at the terminator, so loop-carried values keep their registers through the whole body.
