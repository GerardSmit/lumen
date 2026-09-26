# Performance roadmap: lumen vs node

Status at commit `d8d4e94`. The measurements were taken on 25 Sep 2026:

- node v24.18.1;
- Ryzen 9 5950X, 128 GB, Windows 11;
- lumen-cli release build with the JIT on.

Ratio means lumen time divided by node time, using the minimum of the timed runs. Below 1 means lumen is faster.

## Where we stand

| | lumen | node |
|---|---|---|
| Micro-benchmarks (127) | median 10.0×, geomean 8.9× | 1× |
| Full Puppeteer test (pptr-aot) | 1668 ms / 39.7 MB | 1660 ms / 81.6 MB |
| Empty script | 42 ms / 7.7 MB | 70 ms / 18.8 MB |
| Binary | 11.0 MB (release-small) | 88.3 MB |
| test262 | 53,577 / 53,577 (normal and eager) | – |
| node-compat suite | 1508 / 2919 (51.7%) | – |

**How the micro ratios spread:**

| Ratio | Micros |
|---|---|
| < 1× | 9 |
| 1–1.5× | 7 |
| 1.5–2× | 5 |
| 2–5× | 19 |
| 5–20× | 58 |
| > 20× | 27 |

**Geomean by category:**

| Category | Geomean |
|---|---|
| exceptions | 0.5× |
| numbers | 3.4× |
| strings | 4.9× |
| async | 5.1× |
| regexp | 5.5× |
| arrays | 10.7× |
| props | 11.9× |
| collections | 14.0× |
| calls | 17.5× |
| alloc | 23.3× |

**Where lumen wins:**

| Case | Result |
|---|---|
| `(a+)+b` backtracking | 164 vs 3034 ms |
| new Error | 0.34× |
| throw/catch | 0.37× |
| stack formatting | 0.39× |
| BigInt mod | 0.65× |
| BigInt modpow | 0.66× |
| default string sort | 0.77× |
| `find` with an arrow | 0.78× |
| delete | 0.84× |
| 2.3 MB `.ts` load | 210 vs 327 ms |

**Workloads:**

| Workload | lumen vs node | Ratio |
|---|---|---|
| json_5mb | 114 vs 69 ms | 1.7× |
| text_1mb | 154 vs 26 ms | 5.9× |
| scheduler_200k | 1176 vs 85 ms | 14× |
| nbody_200k | 1807 vs 26 ms | 69× |
| typescript.js via require | 1166 vs 177 ms | 6.6× |

## Roadmap (ordered by gain per effort)

Effort: **S** means a day or less, **M** a few days, **L** a week or more.

1. ✅ **Done: array front-slack (M).**
   - What: keep the first-element offset separate from the allocation start, so shift, unshift and `splice(0, …)` are O(1) amortized.
   - Also fix the push path in the same change: a native in-capacity append in the JIT, and an integer length field instead of the string-keyed length sync.
   - Why: these used to run the spec's per-index loop in `array_fast.rs` at about 32 ns per element.
   - Built:
     - `PackedVec` has front slack, and shift, unshift and `splice` have fast paths.
     - A JIT `push` intrinsic whose guard is cached per receiver.
     - A single-borrow append that finds `length` once.
     - A "flat" proof (no heap handles), so copying and dropping such arrays is a `memcpy` and a free.
     - Fixed along the way: `push` on a non-extensible array now throws, as in node.
   - Release results, lumen vs node:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | queue push+shift | 3559× | 167 ns | 94 ns |
     | unshift+pop | 176× | 254 ns (lumen faster) | 1892 ns |
     | splice | 38× | 997 ns | 897 ns |
     | push | 123 ns | 38 ns | 7.8 ns |
     | slice_1k | ~12 µs | 644 ns | 392 ns |
     | map | 82 ns | 51 ns | 15 ns |
     | filter | 51 ns | 36 ns | 15 ns |
   - Left: push still makes one helper call plus a receiver clone per call; an inline IR append would close the rest.
2. ✅ **Done: compound member assignment in the JIT (S).**
   - What: lower `o.x += v` and `o[i] op= v` through the property IC.
   - Why: it took the generic one-op fallback, at 75–100 ns.
   - Built:
     - `AppendProp` (the fused `o.x += v`) is a number add plus the IC store when both operands are numbers, and the generic op otherwise.
     - `ToPropKey` is lowered, so `a[k] op= v` stays in JIT code.
     - Object and string retains are inline in `Dup` and in refcounted property reads.
   - Release results, lumen (node):

     | Case | Before | Now | Node |
     |---|---|---|---|
     | `o.x += i` | 85 ns | 17.5 ns | 1.3 ns |
     | `a[i & 7] *= c` | 178 ns | 24.7 ns | 1.4 ns |
     | `this.v += i` | 86 ns | 18 ns | 12 ns |
     | nbody_200k | 1807 ms | 1157 ms | 27 ms |
     | scheduler_200k | 1176 ms | 864 ms | 88 ms |
   - Not closed: nbody and scheduler are now limited by general JIT code quality (item 17) and global reads (item 5), not by compound assignment.
3. ✅ **Done: cheaper re-entry into JS (L).**
   - What: push a frame directly for setters, `fn.call`, `apply`, bound functions and promise jobs, instead of going through `Interp::call` (about 200 ns). Cut native entry into JIT code from 40–60 ns.
   - Built:
     - JIT direct call sites take adaptors:
       - `f.call(t, …)` and `f.apply(t, arr)` call `f`'s code directly. The `GetMethod` is validated like a method site, and apply's array is checked and spread into the slots by helpers.
       - A bound function calls its target, guarded on the target rather than the bound object, so a fresh `bind` per call still hits.
     - Accessors:
       - Setters with an inlinable body are inlined, like getters were.
       - Other getters and setters become direct calls behind the shape/holder probe.
     - Sloppy↔strict calls switch strictness around the call, instead of always falling back to `Interp::call`.
     - Native → JS: `jit::call_direct` enters the callee's JIT code on the shadow stack with no `Vec` frame. It is used by `PreparedCall` (map, forEach, sort comparators, replace callbacks, …) and by promise reaction jobs.
   - Release results:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | setter | 273 ns | 7.7 ns | 0.5 ns |
     | non-inlinable getter / setter | 130 / 126 ns | 31 / 26 ns | — |
     | `f.call(o, …)` | 210 ns | 35.5 ns | 8.9 ns |
     | `f.apply(o, arr)` | 332 ns | 76 ns | 18 ns |
     | bound call | 229 ns | 24.8 ns | 10.7 ns |
     | sort 1e5 with comparator | 136 ms | 101 ms | 36 ms |
     | then_chain | 698 ns | 638 ns | 242 ns |
   - Left:
     - `fn_call`'s remaining cost is the global `LoadName` of `f` (item 5).
     - then_chain is dominated by collector passes over the growing live chain, about 200 ns per traced object (item 13).
     - JIT resume after `await` (`LUMEN_JIT_RESUME`) is still off.
4. ✅ **Done: skip building `arguments` and rest arrays (M).**
   - What: read `a.length` and `a[k]` straight from the frame when the object doesn't escape.
   - Built:
     - A virtual `arguments` / rest object (`Chunk::virt_base`) is used when the body only reads `.length` and `[k]` and writes no parameter.
       - The compiler widens `n_params` by a window of 8 hidden parameter slots.
       - The object's slot holds only the element count.
       - `ArgsLen` / `ArgsGet` read the count and the slots.
     - Fallbacks:
       - Any other use, or a parameter write, recompiles with a real object.
       - A call with more arguments than the window materializes the object at entry.
       - A non-index or out-of-range key materializes it on demand.
     - Sloppy functions with simple parameters that use `arguments` used to run on the tree-walker. They now compile whenever the virtual form applies: with no parameter written, mapping is unobservable.
     - JIT:
       - Inline count and element reads, with the generic op as the slow path. The slots are kept in memory.
       - Direct calls seed the count, including rest-parameter callees.
   - Release results:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | `arguments.length + arguments[0]` | 1146 ns | 27.7 ns | 1.0 ns |
     | `(...a) => a.length` | 63 ns | 23.8 ns | 0.87 ns |
   - Left: what remains is the direct call itself. Node inlines these; our inliner still rejects callees with `arguments` or a rest parameter.
5. ✅ **Done: global inline cache (S).**
   - What: a property cell with a shape guard for unqualified `LoadName` and `StoreName`.
   - Built:
     - Interpreter: a free-name store now hits the guarded `NamePath` (script-scope binding or global-object data property) instead of re-resolving every write.
     - JIT:
       - Global-object properties are read and written in place through a cached `Property` address (`Src::Glob`, `Helper::GlobPtr`). It stays valid until JS runs.
       - A read speculates Number when the property held one at translation, and exits otherwise.
       - A write stores a Number or Boolean over a writable data property holding a droppable value.
       - Script-scope `let` / `var` bindings reached through a `NamePath` get binding addresses too (`Src::NameW` for writes).
   - Release results:

     | Case | Before | Now |
     |---|---|---|
     | `globalThis.gv` read | 31 ns | 3.0 ns |
     | `globalThis.gv` write | 371 ns | 3.7 ns |
     | script `var` / `let` write | 12 ns | 2.4 ns |
     | top-level `fib(30)` | 111 ms | 59 ms (node 55) |
6. ✅ **Done: `new.target` in the bytecode compiler (S).**
   - Why: functions that use it were rejected by the compiler and ran on the tree-walker.
   - Built:
     - `Op::LoadNewTarget` pushes the engine's current `new.target`. Every call path already sets it on entry (the constructor on a construct, `undefined` on a call) and restores it on exit, so it is exact in a synchronous non-arrow body.
     - Still on the tree-walker:
       - arrows, async functions and generators that read `new.target`;
       - functions whose inner arrows read it (the capture scan bails).
     - Constructor templates (`ctor_plan`) skip a call guard `if (!new.target) …` (or `== null` / `=== undefined`): under a construct it is dead.
   - Release results:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | ctor_new_target | 1363 ns | 123 ns | 9.5 ns |

   - Left: the plain-constructor allocation cost, which item 8 covers.
7. ✅ **Done: iterators in the JIT (M).**
   - Why: the JIT was slower than the interpreter for for-of (119 vs 55 ns).
   - Built:
     - The JIT loses to the interpreter because of Boxed arithmetic, not the iterator: `s += v` with a Boxed `v` went through two helpers.
       - `arith_local` now does `+ - * /` inline when both operands are Numbers at run time, for an in-memory destination.
       - `binary_num` no longer computes ToInt32 (an `fmod`) for non-bitwise ops.
     - An encoded Array for-of state is stepped inline: index < length, dense data element.
     - Map/Set iterator objects (`keys()` / `values()` / `entries()`) under the intrinsic `next` are stepped in place in `iterator_step`, with no call and no result object.
       - The iterator's internal slots are read by a shape-cached layout, with no per-step key lookups or `Rc<str>` allocations.
       - The JIT helper reports such a step as "no JS ran".
     - `DestructureArr` of a pristine dense Array in JIT code calls `try_dense` directly instead of the generic op.
   - Release results:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | for_of | 119 ns | 19 ns | 12.7 ns |
     | set_iter | 101 ns | 32 ns | 3.5 ns |
     | map_keys | 432 ns | 71 ns | 3.6 ns |
     | map_entries | 410 ns | 128 ns | 9.7 ns |
     | map for-of `[k, v]` | 261 ns | 152 ns | 10 ns |

   - Left:
     - Map/Set entries still allocate the `[k, v]` pair, and each allocation costs about 50 ns; item 8 covers this.
     - Set states and generators still step through the helper.
8. ✅ **Partly done: allocation-heavy loops (M).**
   - Finding: the allocation micros store each new object into a ring array (`r[i & 1023] = {…}`). That element store, not the allocator, was the biggest cost.
     - Any non-Number value (and a Number written into a hole) took the generic op through `run_vm`: 40–50 ns per store.
   - Built:
     - `Helper::ElemSetBoxed` calls `Interp::fast_set_elem` directly. It covers overwriting an existing element (releasing the old value), filling a hole and a dense append, for owned values and Numbers.
     - The generic op remains the fallback for accessors, frozen arrays, proxies and the like.
   - Release results:

     | Case | Before | Now | Node |
     |---|---|---|---|
     | obj_empty | 89 ns | 51 ns | 9.6 ns |
     | obj_xy | 93 ns | 56 ns | 9.3 ns |
     | arr_2 | 101 ns | 66 ns | 10.6 ns |
     | new_class | 125 ns | 85 ns | 9.4 ns |
     | idx_write_new_array | 87 ns | 51 ns | 2.4 ns |

   - Left: the allocator itself.
     - It is already a free-list slab with a const TLS pointer and an inline-props drop.
     - An allocate-and-discard of `{x, y}` costs about 17 ns to allocate and 7 ns to drop, plus the helper calls.
     - Native box initialization from JIT code (an image copy of the template's `RefCell<Object>`, with pointer and refcount fix-ups) would save only the call overhead. The heap state can also switch between coroutine jobs, so the JIT would have to load it per region.
     - arr_1_discard (48 ns vs 0.8) needs escape analysis.
9. ✅ **Done: small lowerings (S each).**
   - Built:
     - Keyed string reads: `Interp::fast_get_str` gives `obj[str]` a fast path on plain objects and their plain prototypes.
       - Own hits are memoized per (shape id, key string identity) in a 64-entry table that holds the key, so its address can't be reused.
       - Used by the VM's `GetElem` and by the JIT's `ElemGetStr` helper.
     - Int32 `%`: two int32 operands with a nonzero divisor take `srem` inline. A zero remainder is `x * 0`, which keeps `-0`, and a divisor of -1 is taken as 1.
     - Nested Math: `Math.f(Math.g(x))` is one inline site. The outer site's guard also checks the inner one, so the inner call can't exit with placeholders on the stack.
     - Array destructuring (`DestructureArr(n ≤ 8)`):
       - A `DestructProbe` helper checks the protectors. `array_iter_return_absent` is now memoized on the proto epoch, with the iterator chain marked as prototypes.
       - The elements are then read natively from packed storage (`layout::packed_prefix` / `packed_word`).
       - All-Number arrays are written straight; anything else goes through `UnpackClone`.
       - Decoding and branching per element cost more than the writes, so both paths are straight-line code.
     - `Object.keys` of a plain object without elements returns the per-shape key list shared with for-in, with no key strings built.
     - Template literals compile to one `Op::Concat(n)`: one allocation, and empty chunks are dropped. In the JIT, `ToStrPrim` and `ConcatStr` helpers replace the generic ops.
     - `MakeRegExp` in the JIT calls a direct helper instead of the generic op.
   - Release results:

     | Case | Before | Now |
     |---|---|---|
     | keyed_string | 184 ns | 42 ns |
     | modulo | 26.8 ns | 3.2 ns |
     | sqrt_floor | 175 ns | 2.5 ns |
     | destructure_3 | 96 ns | 35 ns |
     | object_keys | 491 ns | 245 ns |
     | template_literal | 590 ns | 161 ns |
     | literal_creation | 128 ns | 100 ns |

   - Left:
     - keyed_string is now dominated by refcount traffic on the two global loads.
     - destructure_3 still pays about 15 ns for the protector probe on each iteration.
     - object_keys pays for the generic native call and the array allocation.
     - literal_creation pays for the RegExp side table.
10. ✅ **Partly done: scopes and closures (M).**
    - Findings:
      - A function whose `for (var …)` head variable was captured by a closure never compiled to bytecode. The capture scan bailed on the head's reset, with no bail reason logged, so the whole function ran on the tree-walker.
      - A block-binding read cost about 20 ns in JIT code. It cloned the env `Rc`, and the inlined TDZ error path made the helper's code much worse.
      - `i++` on a block binding went through the generic step-and-store with a temporary `Vec`.
      - `x & 1023` with a Boxed operand merged its slow path as Boxed, so the element store that used it as a key fell back to the generic op.
    - Built:
      - A captured `for (var …)` head now binds like a plain `var` unless it would reset a parameter or a function declaration.
      - Block envs:
        - Access is borrowed, with no `Rc` traffic.
        - Names are looked up by pointer first (they are interned).
        - Reads use a TDZ-free `load_opt`.
        - `++`/`--` on a Number updates in place.
        - `BlkCopy` reuses the env when nothing captured it, and otherwise clones the binding vector wholesale.
        - `BlkNew` and `BlkCopy` retarget the existing carrier instead of allocating a new one.
      - JIT: `- * / & | ^ << >> >>>` with a Boxed operand now produce an unboxed Number; the rare other result exits.
    - Release results:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | arrow_var (capture of a `for (var …)` counter) | 430 ns | 99 ns | 11.8 ns |
      | arrow_let | 316 ns | 207 ns | 16.5 ns |
      | block env, no capture this iteration | 173 ns | 48 ns | – |
      | block_const (`{ const k = i; … () => k }`) | ~225 ns | 192 ns | – |

    - Left:
      - function_expr (258 ns vs arrow 90 ns, node 17 ns) is the `.prototype` object, the fn↔prototype pair marking and release, and the legacy `arguments`/`caller` accessors on sloppy functions. A lazy `.prototype` needs every own-property path to materialize it first, and there are over 1,000 direct `props` accesses across 81 files. It needs a props-level design, not a patch.
      - A closure's `UserCallable` is a separate `Rc` allocation. Storing it inline would add 8 bytes to every object.
      - Per-iteration envs still allocate a `Scope` plus a binding vector, and register a weak entry for the cycle collector.
11. ✅ **localeCompare fast path for ASCII and root collation (S).**
    - Finding: every call constructed an `Intl.Collator`, read its options back as strings, then decomposed both strings into per-character vectors.
    - Built:
      - With no locales or options, `localeCompare` compares directly with the default (`en`) collation.
      - `collate` has an ASCII path, used also by `Intl.Collator#compare`: lowercased bytes, then case bits, with no allocation.
      - Found while measuring:
        - A JIT'd element read feeding `<` speculated a Number and exited to the VM on every iteration when the elements were strings. Such exits are now counted per site; after 16 the site stops speculating and the code recompiles.
        - Number/Boolean operands of arithmetic and non-strict comparisons now convert inline in the JIT (`s += a < b` took the generic op).
        - A comparison with Boxed operands now joins as an unboxed Boolean.
        - String `<` compares bytes up to an ASCII difference.
    - Release results:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | localeCompare micro | 5 µs | 155 ns | 12 ns |
      | `s += w[i] < w[j]` (strings) | 372 ns | 117 ns | 12 ns |
      | `s += k < 5000` | 82 ns | 1.9 ns | – |

    - Left: what remains is the cost of calling any String.prototype method on a primitive receiver (item 18).
12. ✅ **Recursive calls (M).** *(Partly done.)*
    - What: a direct self-call in the JIT and int32 specialization.
    - Finding: the JIT already calls itself directly (shadow stack, lazy frame records), so recursion never reaches `LoadNameForCall` + `CallWithThis`. All of fib's time is inside JIT code. A bare call costs 27.8 ns vs 5.5 ns in node.
    - Built: the direct call clones object arguments and releases a small callee frame (≤ 6 slots) inline, instead of calling a clone helper and `DropN`.
    - Release results:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `sum(a, i)` with `a.length` and `a[i]` | 49 ns/call | 43.5 ns/call | – |
      | fib(30) | 60 ms | 58 ms | 14 ms |

    - Left: the machine cost of the call sequence itself (frame record, flag stores, prologue). This overlaps item 17.
    - `ack(2, 2000)` exceeds the 1,500-frame depth limit (item 15).
13. ✅ **Promise path (M).** *(Partly done.)*
    - What: lighter reaction records, and no `promise_then_is_silent` string lookups.
    - Built:
      - `queueMicrotask` is a native function that enqueues a job directly. It used to be a JS shim around `Promise.resolve().then(cb)`, which cost two promises and a reaction per call. A throw still becomes an unhandled rejection.
      - The pristine checks behind `then`, `Promise.all` and `Promise.resolve` read `constructor`, `then` and `@@species` from slots cached against each object's shape, instead of hashing the keys on every call.
      - Found while measuring:
        - `++x` inside a closure, on a variable of an enclosing scope (`UpdateNameCached`), wrote back through the uncached `assign_free_name` in the VM and took the generic op in the JIT. It now stores through the name cache, and the JIT updates a Number in place through the binding pointer.
        - `obj[i](...)` (`GetMethodElem`) now reads the element natively in the JIT.
    - Release results:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | queueMicrotask | 1,700 ns | 680 ns | 116 ns |
      | `() => { ++c }` called in a loop | 170 ns | 20 ns | 2 ns |
      | `fs[0]()` | 175 ns | 120 ns | 4 ns |
      | then chain (first run / steady build + drain) | 2,400 ns / 630 ns | 2,200 ns / 600 ns | 229 ns / 57 ns |
      | Promise.all | 787 ns | 700 ns | 260 ns |

    - Left, and why:
      - The first run of each benchmark is about 3× its steady state. Every fresh block comes from the system heap, because the size-class cache only recycles freed blocks.
      - A call to a closure the JIT cannot bind directly costs about 100 ns through `helpers::call` → `call_native` → `enter`. This overlaps item 17.
      - `Promise.resolve(x)` still costs about 157 ns: the call path, plus allocating the object and its `PromiseSlot`.
14. ✅ **Parser (L).** *(Partly done.)*
    - What: smaller AST nodes (Function 184 B, Stmt 128 B, Param 112 B), an arena, and memory for the token vector.
    - Closes: parse 3.7–6.6× and a 7–11 MB peak from temporary fragmentation.
    - Finding: the lexer dominated, not the AST.
      - Every punctuator built a `String` and scanned the 60-entry punctuator list.
      - Every identifier was scanned against the keyword list.
      - `Parser::advance` cloned the token it stepped over (a `String` for every identifier and string literal), and no caller used the clone.
    - Built:
      - Punctuators lex through one `match` on the next four chars.
      - Identifiers take their ASCII run in one step, and keywords resolve through a `match`.
      - `advance` no longer clones.
      - The parser's char→byte offset map for non-ASCII sources keeps one checkpoint per 64 chars instead of one `u32` per char: 4 MB → 64 KB for a 1 MB bundle.
    - Release, `new Function(src)` on the 1 MB puppeteer-core browser bundle: 52 ms → 32–35 ms (node 14.6 ms on its first compile).
    - Left:
      - The lexer still works on a `Vec<char>` copy of the source (4 bytes per char).
      - Identifier and string tokens own `String`s that the parser clones in `match self.cur().clone()`.
      - Bodies of lazily parsed functions are still lexed with the rest of the file.
      - The AST node sizes and an arena, as planned.
15. ✅ **Limits (S).** *(Partly done.)*
    - `MAX_LIVE` (3M live objects) throws a RangeError that escapes try/catch; node handles 5M.
    - `MAX_EVAL_DEPTH` allows 1,500 frames, against about 10.4k in node.
    - The 1e6-objects case also takes 462 ms / 178 MB vs 142 ms / 141 MB.
    - Built:
      - `MAX_LIVE` is 20M on native targets, about 3.4 GB at the measured ~170 bytes per small object. wasm32 keeps 3M.
      - The RangeError is catchable. It escaped because the handler's first allocation hit the same ceiling and threw again. The collector now leaves 100k allocations of headroom after throwing.
      - `MAX_EVAL_DEPTH` is 7,000. Measured in release with the ceiling lifted, one unit of depth costs at most about 7 KiB of native stack: an async function recursing on a 64 MiB coroutine thread. JIT'd recursion costs about 3 KiB, and tree-walker `map` recursion about 4.5 KiB. 7,000 therefore stays inside the smallest (64 MiB) engine thread.
    - Release results:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | 5M live objects | uncaught RangeError | 1,970 ms / 780 MB | 212 ms / 362 MB |
      | recursion depth | 1,500 | 6,994 | 10,420 |

    - Tail calls are no longer tied to the ceiling. `TAIL_NEST` was `MAX_EVAL_DEPTH / 16`, so raising the ceiling let tail recursion run about 440 ordinary native frames before the trampoline took over. That overflowed a 2 MB test thread (a spread tail call). It is now a fixed 96.

    - Left:
      - The 1e6-objects case is unchanged at 403 ms / 169 MB (node 67 ms / 135 MB).
      - About half of that is the cycle collector. Each pass is a full trial deletion over every live object at 100k, 200k, 400k and 800k live objects, about 120 ns per object.
      - A generational or incremental scheme, or skipping objects that cannot be part of a cycle, is the next step.
16. ✅ **module.js (M).** *(Partly done.)*
    - What: generate the 52 builtin ESM wrappers on demand. Move the CJS resolver into Rust and share it with `esm.rs`, which gains `exports` subpath patterns.
    - Saves: about 0.8 MB of startup memory.
    - Built: the node glue no longer builds every builtin's synthetic ESM source at startup. It hands the runtime each builtin's export-name list (`__esmExportLists`), and the loader builds a `node:x` source from its list when `node:x` is first imported (`esm::builtin_source`).
    - Result (empty script, mem-stats build):

      | | Before | Now |
      |---|---|---|
      | private memory | 8,664 KB | 7,712 KB |
      | allocator live | 5,536 KB | 5,056 KB |
      | allocations | 119,212 | 92,847 |

    - Left: the CJS resolver move. It is a compatibility refactor (shared resolution, `exports` subpath patterns) rather than a memory or speed win, so it is listed under Node compat.

17. ✅ **JIT values in registers (L).** *(Added during item 2. Partly done.)*
    - What: keep object references loaded from properties in SSA instead of boxing them through stack memory, and elide retain/release pairs whose lifetimes nest (for example `const v = b.v` releases and re-takes the same count every iteration).
    - Also: shorten the inline-cache guard chain of dependent loads for a repeated receiver, and avoid byte-store/word-load store-forwarding stalls in the emitted code.
    - Evidence: `b.v.x -= c` takes 21 ns vs 1.2 ns, and `const v = b.v` 12 ns vs 0.75 ns. Nearly all of nbody's time is inside JIT code.
    - Built:
      - Chained reads (`a.b.c`): when a property read feeds straight into another plain read, the inline cache's object is lent to the second read without taking a reference (a `Src::Borrow` entry). Anything else (a miss, a getter, a primitive) reads the usual way; both paths join after the second read. Chains do not nest, so code size stays linear.
      - Borrowed receivers for plain stores (`b.w += 1`, `b.v.x = k`): the store writes through the receiver where it lives instead of first cloning it onto the stack.
      - Forcing a borrowed value onto the stack clones it inline instead of calling the `Clone` helper.
      - A `let`/`const` slot's TDZ marker is not written when its initialising store follows in the same block with nothing in between that can observe it.
    - Release, per iteration in a JIT loop (nbody: 200k steps):

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `b.v.x` read | 12.7 ns | 7.3 ns | 0.78 ns |
      | `b.w += 1` | 17.1 ns | 12.1 ns | 1.36 ns |
      | `v.x+v.y+v.z` | 37.0 ns | 34.8 ns | 0.83 ns |
      | `b.v.x -= c` | 20.7 ns | 20.7 ns | 1.33 ns |
      | `const v = b.v` | 13.8 ns | 13.4 ns | 0.78 ns |
      | nbody | 492 ms | 427 ms | 27 ms |

    - Left:
      - Every property access still runs the full guard chain: tag, borrow flag, exotic check, shape, entry count and the accessor check.
      - A per-slot cache of the validated receiver and shape, dropped at slot writes and calls into JS, would let repeated accesses on one receiver skip it.
      - `const v = b.v` still takes and releases a reference per iteration.

18. ✅ **Method calls on primitive strings (M).** *(Added during item 11. Partly done.)*
    - Finding: any non-intrinsic String.prototype method costs 115–150 ns per call from JIT code (node: 11–20 ns). Examples: `at` 115 ns, `startsWith` 131 ns, `indexOf` 151 ns.
    - The time is spread across four steps:
      - the `GetMethod` helper resolving through the String.prototype IC;
      - the generic `call` helper and `call_native_fast`;
      - `this_string`;
      - the arguments' ToString.
    - What: extend the `strm` plan (today only `charCodeAt`) to call any String.prototype builtin's native entry directly. Guard the receiver's type and the prototype's method identity at the `GetMethod`, and pass `this` and the arguments without the generic call frame.
    - Built:
      - Fused sites (`strn` plan): `<recv>.m(args)` where `m` is a native `String.prototype` method and the arguments are pure loads or Number arithmetic. The `GetMethod` only checks that the receiver is a String; a non-String receiver exits once and the next compile leaves the site to the generic path.
      - At the call, one helper (`str_method`) reads the method, calls the native directly, and writes the result. The read uses a String.prototype slot cached by shape; the value is re-checked on every call. Because the arguments cannot run JS, reading the method after them is unobservable. A patched or getter method takes the ordinary `GetMethod` + call steps.
      - Shared fast paths (`builtins::str_fast`) for `at`, `charAt`, `startsWith`, `endsWith`, `includes`, `indexOf`, `slice`, `trim*` and ASCII `toUpperCase`/`toLowerCase`. They apply when the receiver is ASCII and the arguments are Strings and Numbers, so nothing can run JS or throw. The natives use them too, and the JIT helper calls them without the native-call bookkeeping. A short needle is searched with a plain scan.
    - Release, per call in a JIT loop:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `at` | 105 ns | 55 ns | 11 ns |
      | `startsWith` | 107 ns | 46 ns | 7 ns |
      | `indexOf` | 138 ns | 67 ns | 1 ns |
      | `slice` | 127 ns | 62 ns | 10 ns |
      | `charAt` | 109 ns | 53 ns | 4 ns |
      | `toUpperCase` | 124 ns | 76 ns | 1 ns |

    - Left: about 22 ns is the floor of any helper call from a JIT loop (boxing, `call_js` invalidation, the status check). Closing the rest needs these methods inline in the emitted code, as `charCodeAt` is, or node-style hoisting of loop-invariant calls.

19. ✅ **Calls to closures (M).** *(Added after the second report. Partly done.)*
    - Finding: the largest remaining gaps were call-shaped. Node inlines small callees; lumen paid a full call (about 20 ns) whenever the callee read a captured variable, `arguments` or a rest parameter. A callee that needs an activation scope (any function whose locals are captured) skipped the direct call path entirely and went through the generic helper call.
    - Built:
      - **Captured reads in inlined callees.** An inlined body may read captured variables (`LoadName`) that resolve in the callee's own closure scope, holding a Number or Boolean. The site checks the scope's generation, the binding's TDZ flag and its value's kind before the body, on every execution: an in-place store can change a kind without running JS. The body then reads the value in place. A miss retires the site. A local-callee site with captures requires the exact planned closure, because another closure of the same code (for example one per loop iteration) closes over other bindings.
      - **Virtual `arguments` / rest in inlined callees.** When the compiler already keeps them virtual (only `.length` and `[k]` reads), the call site knows the argument count. `.length` becomes a constant and `[k]` with a constant `k` becomes the argument itself.
      - **Direct calls into callees with an activation.** The call record gained a third 16-byte unit that holds the call's owned activation environment. `Helper::ActEnv` builds it from the seeded parameter slots, the frame's `env` points at it, and `Helper::DropEnv` releases it after the call on every path. `LUMEN_NO_DIRECT_ACT` turns this off.
    - Fast profile, per call in a JIT loop:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `add(s)`, `add = mk()` capturing `const k` | 24.8 ns | 3.1 ns | 0.9 ns |
      | `restf(i,i)` + `argsf(i,i)` + `restg(i,1,2)` | 94.6 ns | 3.9 ns | 2.2 ns |
      | `mk(i)`, returning `() => x` | 286 ns | 148 ns | 9.3 ns |
      | `mk2(i)`, two captured locals | 336 ns | 188 ns | 11 ns |

    - Left:
      - A capturing call still makes four allocations: the scope, its binding vector, the function object and its `UserCallable`.
      - A function expression also builds its `prototype` object eagerly (node does this lazily).
      - A `let` captured in a loop body allocates a scope per iteration (about 140 ns).

20. ✅ **Map / Set method sites (S).** *(Added after the second report.)*
    - Finding: `Map.prototype.get` is the third most called builtin in a Puppeteer session, and `get`/`has`/`set` ran 15× slower than node: a generic `GetMethod`, then a native call with its bookkeeping, then the hash lookup.
    - Built: fused sites (`colm` plan, like item 18's `strn`) for `<object>.get|has|set|add(args)`, where the arguments are pure loads or Number arithmetic. The `GetMethod` checks only that the receiver is an object. At the call, one helper (`coll_method`) reads the method through the receiver's and prototype's shapes (cached per site; the value is re-read every call). When that method is the intrinsic Map/Set native and the receiver holds that kind of collection, the lookup or insert runs inline (`collections::lookup::coll_fast`). Any other native is called directly; user methods, getters, own properties and patched prototypes take the ordinary steps. A site that the planner already turned into a direct JS call (a user class's `get`) keeps that path.
    - Fast profile, per call in a JIT loop: `get` 91 → 56 ns, `has` 85 → 49 ns, `set` 98 → 61 ns. A user class with `get`/`set` methods is unchanged (35 ns).
    - Left: the same helper-call floor as item 18. A key that is not a pure load (`m.get(a[i])`, `m.get(s + x)`) still takes the generic path.

21. ✅ **Destructuring, spread calls, collection iterators (S).** *(Added after the second report, weighted by what puppeteer-core uses: 143 object destructurings, 115 spread calls, 27 array destructurings, 12 `for…of` over `.keys()` / `.values()` / `.entries()`.)*
    - Built:
      - **`DestructureGuard` in the JIT.** It was a generic-helper op, and it forced its operand boxed, so every `const {a, b} = o` cloned and dropped `o` and made a helper call. It is now one tag check (exit for `undefined` / `null`), reading a lent source in place.
      - **Array destructuring of a lent array.** The inline `DestructureArr` path reads a borrowed array in place, and clones it only for the slow path.
      - **Spread calls in the JIT (`Helper::CallSpread`).** `f(...a)` / `o.m(...a)` used the generic op (the interpreter's call chain plus a fresh VM drive). The helper gathers the arguments into a stack buffer (a pristine packed Array as a straight copy, anything else by the iterator protocol) and then dispatches as `Helper::Call` does, including the direct entry into compiled callees.
      - **Fresh Map/Set iterators run encoded.** `GetIter` on a `m.keys()`-style iterator that nothing else references (refcount 1, only its internal slots, intrinsic `next` and `@@iterator`) swaps it for the encoded state already used for `for (x of set)`. The object is unobservable, so nothing can tell the difference.
    - Fast profile, per operation:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `const {x, z} = o` | 54.9 ns | 7.7 ns | 0.7 ns |
      | `const [a, b, c] = A3` | 26.0 ns | 20.2 ns | 2.7 ns |
      | `f3(...A3)` | 320 ns | 170–190 ns | 20 ns |
      | `for (k of m.keys())`, per key | 64.3 ns | 30.3 ns | 4.8 ns |

    - Left:
      - The generic call floor. `Math.max(1, 2, 3)` costs 110 ns: about 40 ns re-resolving the global `Math` after each call (the JIT's cached binding address is invalidated by any call), and the rest is native-call bookkeeping. A spread call pays the same floor.
      - Array destructuring still proves Array iteration pristine through a helper on every run.
      - `for (const [k, v] of map)` allocates an entry array per step (about 100 ns per entry).

22. ✅ **Guards, tests and small builtins that were still generic (S).** *(Added after the third ranking: the largest remaining ratios among operations common in Puppeteer and VS Code extension code.)*
    - Built:
      - **Absent-property IC.** A read that misses has an inline shape chain that ends at `null`: a feature test like `if (o.x)`, an options default, or an absent method on an array or string wrapper (a non-index name other than `length`). The IC used to take the full lookup every time.
      - **`in` and `hasOwnProperty`.**
        - A string key against an ordinary object or array takes a raw-pointer walk of own properties and prototypes (`Interp::js_has_property`).
        - `hasOwnProperty` checks the props map directly.
        - The JIT calls `Helper::Binary` with `BIN_IN` instead of the generic op.
      - **Unary `+` / `-` on a non-Number** go through `Helper::Binary` (`BIN_PLUS` / `BIN_NEG`). A string operand goes straight to `str_to_number`, with an all-digits fast path for strings of up to 15 characters.
      - **Integer ToString.** `String(n)`, `"" + n` and template substitutions of an integral Number below 2^53 write digits into a stack buffer (`eval::int_str`), with no formatter and no temporary `String`.
      - **Inline truthiness.**
        - `if (v)` on a Boxed value decides Boolean, Number, `undefined`/`null`, String (by length) and objects with `ic_plain` from the tag. Only BigInt, Symbol and possible `[[IsHTMLDDA]]` objects call `ToBoolean`.
        - `JumpIfFalse` and `TypeofIs` read a borrowed local in place, without a clone and release.
      - **Loose equality.**
        - `x == null` / `x != null` is decided by the tag, both for a memory operand and for a `null` constant pushed just before.
        - Two operands of the same type compare strictly.
      - **`typeof x === "…"`** is decided by the tag for every primitive. An object goes through a small `Helper::TypeofIs`, which checks function-ness and `[[IsHTMLDDA]]`, instead of the one-op VM.
      - **Element reads with a Boxed result** do one element lookup that decodes any element kind. Previously a Number lookup ran first, and every non-number element repeated the walk.
      - **Dead TDZ markers.** `const v = a[i % n]` in a loop body no longer writes and releases its slot twice per iteration: `tdz_dead` now accepts `GetElemLocal` and local arithmetic between the marker and the store.
      - **`arguments` objects** are cloned from a per-argument-count template, one per realm, mapped or unmapped. Each call patches only the values and a mapped object's `callee`. The old builder allocated one key string per index and walked shape transitions for every property.
      - **Call-site guards of inlined free-name callees** compare the binding's object pointer in place (`Chunk::name_ic_obj_ptr`). They no longer clone and release the function object on every call.
    - Fast profile, per operation in a JIT loop:

      | Case | Before | Now | Node |
      |---|---|---|---|
      | `o.missing` (plain / array / string wrapper) | 36 / 145 / 112 ns | 14 / 17 / 30 ns | — |
      | `'a' in o` | 166 ns | 48 ns | — |
      | `o.hasOwnProperty(k)` | 199 ns | 126 ns | — |
      | `+str` | 108 ns | 38 ns | 7 ns |
      | `x == null` | 75 ns | 33 ns | 2 ns |
      | `typeof v === "string"` / `"object"` | 91 / 92 ns | 25 / 25 ns | 3 / 4 ns |
      | `const v = a[i % 7]; if (v) c++` (objects) | 35 ns | 22 ns | 4.6 ns |
      | `w(1, 2, 3)` doing `f3.apply(this, arguments)` | 1689 ns | 890 ns | 1.3 ns |
      | `w3(i, 1, 2)` calling a global `f3` | 40 ns | 34 ns | 1.3 ns |

    - Left:
      - **Non-leaf calls.** A callee that itself calls cannot be inlined, so it pays the direct-call floor plus a per-call guard for each inlined callee inside it. The guard re-validates the callee's global binding through the name cache in the callee's own frame. Hoisting it needs an invalidation epoch on binding writes. This is the largest remaining gap on call-heavy code: Richards, DeltaBlue, event emitters.
      - **Rest and `arguments` forwarding.**
        - `(...args) => f(...args)` still builds the rest array, because a spread use makes a virtual rest escape: 295 ns vs 1.7 ns in node.
        - `apply` over an `arguments` object takes the generic array-like path (about 430 ns).
      - **Allocation.** `{}` costs about 46 ns (node: 11 ns): `make_lit` → `make_plain_object_vm`, `Object::new`, the element store through `elem_set_boxed`, and `gc_drop_slow` for the replaced value. An arrow function costs 80 ns and a `function` expression 268 ns, the latter because of its eager `prototype`.

## Open items

### Engine
- ptc/t2: the `args` and `closure` rows are slower with the JIT on than off (2090 vs 1505, 637 vs 378).
- BigInt has about 200 ns of interpreter overhead per op; a 2048×2048 multiply takes 1.7 vs 0.39 µs.
- The SyncFn general call path is designed in [sync-callbacks.md](sync-callbacks.md) but not built.
- Typed tier Stage 1: still need to confirm the JIT consumes the `lumen::typescript::type_table` hints.
- Native frame records cost 2.5–3% on native-call-heavy loops; closure creation is 3–4% slower.
- Lint debt: 107 pre-existing clippy warnings under `-D warnings`, and `cargo fmt --check` is not clean.

### Semantics and divergences from node
- **Proper tail calls:**
  - Strict self-recursion now loops forever. That is spec-correct, but node throws a RangeError, so `test-console-no-swallow-stack-overflow` times out.
  - Tail calls through `fn.call`, `apply` and bound functions still overflow.
- **Inline callbacks** diverge from node when:
  - a Proxy is inserted into the prototype chain mid-loop (trap order);
  - `g.caller` is read from an inlined sloppy arrow;
  - an element getter runs (it sees an extra arrow frame);
  - the callback throws before its first call (the arrow frame has no line:col).
- **`f.arguments`:**
  - The first read can miss an activation record when the key is computed.
  - The property is an accessor, not a data property.
  - `defineProperty` with a value throws where V8 accepts it.
- **Stack traces:** the first trace after startup can name a subclass receiver's frame `Array.map` instead of `A.map`.

### Memory (Puppeteer, about 40–50 MB)
- Source and AST: 5.5–8.4 MB.
- Parser temporaries: 7–11 MB.
- Allocator cache: up to 3.5 MB.
- Startup glue objects: 2.2 MB, of which module.js is about 0.8 MB.
- One 1 MB slab chunk.
- Idea: a codec that carries class definitions, so glue closures that contain classes can also be precompiled.

### TypeScript
- The checker re-parses the source with its own TypeScript parser (about 4,900 lines) on the first `type_table()` call. The fix is to port it to the engine AST with a side table (a few days).
- `ts_relex` re-lexes to the end of the file each time. This is rare but can be quadratic.
- Error messages for invalid TypeScript are worded differently from node's; the codes and frames match.

### Node compat
- `fs.watch` polls every 50 ms and misses short-lived files.
- `cluster-shared-leak` times out (half-open sockets).
- Missing or stubbed: Worker `resourceLimits`, `HTTPParser.consume`, and dgram under cluster (returns ENOTSUP).
- `AsyncResource.emitDestroy` is a no-op, so the gc-* tests wait forever.
- Slow tests: `stringbytes-external`, `v8-serialize-leak`, `util-inspect-long-running`.
- Slower with the JIT on: typedarray deepEqual, http-pipeline, and http2 memory-leak.
- `promises-unhandled-rejections` still fails.
- Planned: lumen-http2 on `h2`, and lumen-tls on `rustls`.
- The CommonJS resolver lives in module.js and supports only the `"."` entry of package `exports`, while `esm.rs` resolves separately without subpath patterns (`"./*"`). Moving the resolver into Rust and sharing it would fix both (left from item 16).
- `passing.txt` is out of date:
  - 890 passing tests are not listed;
  - 3 listed tests now fail: http-pause-no-dump, process-constrained-memory, net-listen-shared-ports.
