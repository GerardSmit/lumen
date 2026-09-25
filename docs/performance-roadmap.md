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

1. **Array front-slack (M).**
   - What: keep the first-element offset separate from the allocation start, so shift, unshift and `splice(0, …)` are O(1) amortized.
   - Also fix the push path in the same change: a native in-capacity append in the JIT, and an integer length field instead of the string-keyed length sync.
   - Why: today these run the spec's per-index loop in `array_fast.rs` at about 32 ns per element.
   - Closes: queue push+shift 3559×, unshift+pop 176×, splice 38×, push 18× (123 vs 7 ns), and the map/filter result append.
2. **Compound member assignment in the JIT (S).**
   - What: lower `o.x += v` and `o[i] op= v` through the property IC.
   - Why: it takes the generic one-op fallback today, at 75–100 ns.
   - Closes: most of nbody 69× and scheduler 14×.
3. **Cheaper re-entry into JS (L).**
   - What: push a frame directly for setters, `fn.call`, `apply`, bound functions and promise jobs, instead of going through `Interp::call` (about 200 ns). Cut native entry into JIT code from 40–60 ns.
   - Also unlocks JIT resume after `await`, which is off today behind `LUMEN_JIT_RESUME` because it is a net loss.
   - Closes: setter 630×, fn.call and bound 24×, apply 19×, the `then` and Promise costs, and non-inlined callbacks.
4. **Skip building `arguments` and rest arrays (M).**
   - What: read `a.length` and `a[k]` straight from the frame when the object doesn't escape.
   - Closes: arguments 1387× (1.09 µs per call) and rest params 88×.
5. **Global inline cache (S).**
   - What: a property cell with a shape guard for unqualified `LoadName` and `StoreName`.
   - Closes: global write 718×, global read 82×, and top-level `fib` in plain `lumen` (111 vs 55 ms).
6. **`new.target` in the bytecode compiler (S).**
   - Why: functions that use it are rejected by the compiler and run on the tree-walker.
   - Closes: ctor_new_target 143×.
7. **Iterators in the JIT (M).**
   - What: direct `IterStep` lowering for Map, Set, entries/keys and generator iterators. Fix the handler-region lowering, so a for-of body isn't compiled through generic helpers.
   - Why: the JIT is currently slower than the interpreter for for-of (119 vs 55 ns).
   - Closes: map_keys 124×, map_entries 42×, map for-of 26×, for_of 9.4×.
8. **Inline slab allocation from JIT code (M).**
   - What: pop the free list and initialize the box natively.
   - Needs: the heap to expose each size class's free-list head and the box layout.
   - Expected: `{…}`, `[]` and `new` from 35–85 ns down to 8–15 ns.
   - Closes: alloc 9–13×, new_class 13×, the arr_1_discard 55× case, idx_write_new_array 36×.
9. **Small lowerings (S each):**
   - keyed string IC (11.6×);
   - int32 `%` (18×, about 40 vs 3.9 ns);
   - Math intrinsic on non-simple operands (sqrt_floor 77×);
   - plain-array destructuring without the iterator protocol (63×);
   - cached key lists for `Object.keys` (28×);
   - a template-literal builder (21×);
   - regex literal per call site (17×).
10. **Scopes and closures (M).**
    - What: block scopes with a fixed layout, instead of a hashed var map plus a full copy per iteration. A lazy `.prototype` on function expressions.
    - Closes: arrow_capture_let 22× (310 → about 120 ns) and function_expr 21× (about 300 vs 92 ns for an arrow).
11. **localeCompare fast path for ASCII and root collation (S).** Closes 436× (5 µs per compare).
12. **Recursive calls (M).**
    - What: a direct self-call in the JIT and int32 specialization.
    - Why: each recursion is a full `LoadNameForCall` + `CallWithThis`.
    - Closes: fib 5.8×.
13. **Promise path (M).**
    - What: lighter reaction records, and no `promise_then_is_silent` string lookups.
    - Closes: then chain 3×, Promise.all 6.7×, resolve 10.8×, queueMicrotask 17.5×.
14. **Parser (L).**
    - What: smaller AST nodes (Function 184 B, Stmt 128 B, Param 112 B), an arena, and memory for the token vector.
    - Closes: parse 3.7–6.6× and a 7–11 MB peak from temporary fragmentation.
15. **Limits (S).**
    - `MAX_LIVE` (3M live objects) throws a RangeError that escapes try/catch; node handles 5M.
    - `MAX_EVAL_DEPTH` allows 1,500 frames, against about 10.4k in node.
    - The 1e6-objects case also takes 462 ms / 178 MB vs 142 ms / 141 MB.
16. **module.js (M).**
    - What: generate the 52 builtin ESM wrappers on demand. Move the CJS resolver into Rust and share it with `esm.rs`, which gains `exports` subpath patterns.
    - Saves: about 0.8 MB of startup memory.

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
- `passing.txt` is out of date:
  - 890 passing tests are not listed;
  - 3 listed tests now fail: http-pause-no-dump, process-constrained-memory, net-listen-shared-ports.
