# RyuJIT lowering / codegen / emitter lessons (from Satori's src/coreclr/jit)

Ranked by speed impact for a simple SSA + LSRA backend. Paths are relative to `src/coreclr/jit`.

## Tier 1
1. **Compare→flags fusion** (`TryLowerConditionToFlagsNode`, lower.cpp:4890)
   - A single-use compare feeding a branch or select, in the same block, becomes a flags compare
     placed right before its user.
   - Rewrites (`OptimizeConstCompare`, lower.cpp:4211):
     - `(x&y)==0` → `test`
     - `x & (1<<y)` → `bt`
     - `relop == 0` → the inverted relop
     - comparing an op that already set the flags against 0 → drop the compare
   - The emitter keeps a peephole that remembers which flags the last instruction set
     (`AreFlagsSetToZeroCmp`, emitxarch.cpp:1585), so a redundant `cmp` is not emitted.
   - `cmp r,0` → `test r,r`.
   - `x<0` as a value → `shr r,31`.
   - Otherwise, xor-zero the destination *before* the compare, then `setcc`.
2. **ARM64 compare-and-branch**
   - `x==0` → `cbz`/`cbnz`.
   - A sign test or a single-bit test → `tbz`/`tbnz`.
   - Ranges: `tbz` reaches ±32KB, `cbz` and `b.cond` ±1MB. For a target out of range, invert
     the branch and jump over a `b`.
3. **Containment**
   - Fold immediates that fit in imm32. Fold a load into its user only when it has a single use
     and nothing between them can write memory, trap or call (`IsSafeToContainMem`,
     lower.cpp:220).
   - **Register-optional operands**: LSRA may leave an operand unassigned, and codegen then uses
     its stack slot directly as the r/m operand.
   - For commutative ops, swap operands so the immediate or memory operand is the one folded.
   - Other x64 idioms:
     - `imul r, r/m, imm32` (three operands).
     - Store 0 as `xor r,r` + `mov [m],r`.
     - RMW memory ops `add [m], r` for globals and counters.
   - ARM64 folds:
     - `madd`/`msub`/`mneg`
     - a shifted-register operand on add/sub/logic/cmp
     - an extended-register operand (`add x0,x1,w2,sxtw`)
     - `bic`/`orn`/`eon`
     - `cmn`
     - an `fcmp #0.0` operand
     - Immediate forms need a legality check: add takes imm12 (optionally `lsl 12`); logic ops
       take a bitmask immediate.
4. **Address modes**
   - x64: `[b + i*s + d32]`.
   - ARM64: `[x + imm12*size]`, `[x + x, lsl #log2(size)]` (the scale must equal the access
     size), or `[x, w, uxtw #s]`.
     - For wasm memory: `ldr w0, [xMemBase, wAddr, uxtw]`, so the address needs no separate
       zero-extend.
   - x64 also turns an `add` whose destination differs from both sources into `lea`, since
     `lea` is non-destructive.
5. **Bounds checks**
   - Emit `cmp idx,len; jae TRAP`: one unsigned compare also catches negative indices.
   - Elimination, from cheapest to most expensive:
     1. Remove a check dominated by the same check, walking the dominator tree with a fact set
        keyed on SSA values (assertionprop.cpp:5600).
     2. Range analysis from branch facts plus monotonic induction variables
        (rangecheck.cpp), under a compile-time budget.
     3. Range-check cloning: turn `a[i]..a[i+k]` into one guard `i+k < len` plus a fast path
        (rangecheckcloning.cpp).
   - **For wasm: a 4GB reservation plus guard pages on 64-bit hosts removes the checks
     entirely.**
6. **Shared cold trap stubs** (`fgCreateAddCodeDsc`, flowgraph.cpp:3566)
   - Keep one stub per trap kind, placed after the function body.
   - The hot path is then just `cmp; jcc rel32`, predicted not taken.
   - Deopt exits use the same pattern.

## Tier 2
7. **Float compares** (x64, after `ucomisd`)
   - Unordered sets ZF, PF and CF.
   - `FEQ` = `jp skip; je T`.
   - `FNE` (unordered, i.e. JS `!=`) = `jp T; jne T`.
   - `FGT` = `ja`, `FGE` = `jae`.
   - Handle `LT`/`LE` by **swapping the operands** into `GT`/`GE`, never with `jb` plus a parity
     check.
   - `x != x` → `ucomisd x,x; jp`.
   - ARM64 after `fcmp`:
     - `FNE` (ordered) = `gt || lo`.
     - `FEQU` = `eq || vs`.
     - `LT` → `mi`, `LE` → `ls`.
   - A two-flag condition used by a select needs two `cmov`/`csel`.
8. **Float idioms**
   - Constants: 0.0 → `xorps`; all ones → `pcmpeqd`; anything else → a RIP-relative,
     deduplicated constant pool.
   - ARM64 constants: `fmov d, #imm8` for ±n/16·2^e; `movi v.2d, #0`.
   - Negate/abs: `xorps`/`andps` with a 16-byte-aligned mask constant.
   - `roundsd` immediates: nearest 4, floor 9, ceil 10, trunc 11. ARM64 uses
     `frintn/m/p/z`.
   - **Always `xorps dst,dst` before `cvtsi2sd`** to break the false dependency (a 2× effect on
     SpectralNorm).
   - `u64→f64`: if the sign bit is set, `shr 1`, OR the low bit back in, convert, then double
     the result.
   - VEX encoding:
     - Use the three-operand forms.
     - The 2-byte `C5` prefix only works when r/m is xmm0-7.
     - For commutative ops, swap operands so that xmm8-15 is not in the r/m slot.
   - min/max: x64 `minsd`/`maxsd` return the second operand on NaN or equal inputs, so wasm and
     JS semantics need a fixup sequence. ARM64 `fmin`/`fmax` already match wasm.
9. **Select**
   - x64: `mov dst, f; cmovcc dst, t`. If `f` is already in `dst`, swap the operands and invert
     the condition.
   - ARM64:
     - `csel`
     - `csinc`/`csinv`/`csneg` for `c?a:b+1`, `c?a:~b`, `c?a:-b`
     - `cinc` when the two constants differ by 1
     - `cset` for `c?1:0`
     - `xzr` for a zero operand
     - `ccmp` chains for `&&`/`||` (lower.cpp:12972)

## Tier 3: integer idioms
10. **Division by a constant**
    - Unsigned:
      - d = 2^k: shift for the quotient, `and` for the remainder.
      - d above half the range: quotient = `x >= d`.
      - Otherwise a magic multiply. A 32-bit udiv on a 64-bit target zero-extends, does one
        `imul r64, magic`, then `shr 32+s`: no RAX/RDX (lower.cpp:8283).
    - Signed: the magic multiply with the ±x fixup, `sar`, and `+ (t>>>31)`.
    - Signed d = 2^k on x64: `lea t,[x+d-1]; test x,x; cmovns t,x; sar t,k`.
    - Remainder: `x - q*d`; on ARM64 `msub`.
    - Runtime division: `xor edx,edx` or `cdq`/`cqo`, then `div`/`idiv`.
11. **Multiply by a constant**
    - ×3/5/9 → `lea [x+x*s]`.
    - ×2^n → `shl`.
    - Anything else → `imul r,r/m,imm32`.
    - ARM64: `add x, x, x, lsl n`; `smull`/`umull` for widening multiplies.
12. **Shifts**
    - Drop a `& 31` or `& 63` on the count, since the hardware masks it.
    - `x<<1` → `add r,r`.
    - `x<<2/3` into another register → `lea`.
    - **BMI2 `shlx/sarx/shrx`** for variable shifts (avoids RCX, three operands); `rorx` for
      constant rotates.
    - ARM64: `rol` becomes `ror` by the negated amount; `ubfx`/`ubfiz`/`sbfiz`.
13. **BMI1 and ARM64 logic**
    - `andn`, `blsr` (`x&(x-1)`), `blsi` (`x&-x`), `blsmsk`, `bzhi`.
    - ARM64: `bic`/`orn`/`eon`.
14. **Extends**
    - `i64.extend_i32_u` = `mov r32,r32`, which is free after coalescing.
    - `movsxd`; `movzx` for small loads.
    - Remove a narrowing cast in front of a compare.

## Tier 4: emitter
16. **Deferred encoding**
    - Each block is a `Vec<Inst>`; jumps are recorded as fixups.
    - Encode only after jump sizes are known.
    - Remove a jump to the next block.
    - Invert a conditional branch whose taken target is the fall-through block.
17. **Jump sizing**
    - Start with every jump long, then shrink to rel8 in a loop until nothing changes.
    - A backward jump can be sized at emit time.
    - Encodings: `7x rel8` / `0F 8x rel32` for jcc, `EB`/`E9` for jmp.
    - ARM64 `b` is never shrunk.
18. **Loop alignment** (late, small gain)
    - Only innermost loops with no call, weight ≥ 3 and size ≤ 96 bytes, aligned to 32 bytes.
    - Put the padding after an earlier unconditional `jmp`.
    - Use the multi-byte `0F 1F` NOPs.

## Prolog and epilog
- x64:
  - A frame of 8 bytes → `push rax`.
  - Under a page → `sub rsp,N`.
  - Larger → probe every page in order (Windows stack guard page).
- ARM64:
  - Allocate the frame with `stp fp,lr,[sp,#-N]!`.
  - Save callee-saved registers in `stp` pairs; the epilog mirrors this with `ldp`.
- JS/wasm stack-overflow check: `cmp rsp,[limit]; jb cold_stub` at entry, with the stub in the
  cold area.
