# Sync non-escaping callbacks

A *sync non-escaping callback* is a function argument that a native calls only while that
native is running, and never stores, returns, or hands to a job. Examples: the callbackfn of
`Array.prototype.map`, a `sort` comparator, a `replace` function, the `JSON.parse` reviver.

The closure passed in such a slot is unobservable once the native returns. This lets an optimizing tier:

- skip allocating the closure,
- run the callback's body in the caller's frame,
- keep its captured variables in the caller's registers.

This note describes what lumen already does in the bytecode compiler, and what the JIT can build on top of it.

## 1. What exists

### 1.1 Bytecode lowering (`bytecode/inline_callback.rs`)

The compiler inlines a call of this shape:

```js
<expr>.<m>(<arrow literal>[, init])
```

The rewrite applies only when all of these hold:

- `m` is one of: `map`, `forEach`, `filter`, `some`, `every`, `find`, `findIndex`, `findLast`, `findLastIndex`, `reduce`.
- The arrow is not `async` and has simple parameters.
- The arrow contains no direct `eval`.
- No nested closure captures the arrow's own parameters or locals.

The compiler then emits the loop directly into the enclosing function:

```text
<obj> GetMethod(m) [extra → sX]
ArrayCbGuard(kind) JumpIfFalse(fallback)      ; the guard, once per call
  Pop StoreLocal(a)  a.length → len
  loop: GetElem a[k] (+ ArrayCbHas only when it read undefined) → params → body …
  ArrayCbDone(call position) Jump(end)
fallback: MakeClosure(arrow) [LoadLocal sX] CallWithThis   ; the unchanged generic call
end:
```

Three details of the lowering:

- The arrow's parameters and its `var`s become parent slots, allocated per call site. See §2.2.
- A `return` inside a block body becomes a jump to the per-element continuation.
- Loop semantics follow the spec step by step:
  - `HasProperty` is checked before `Get`.
  - `length` is read once.
  - `some`, `every` and `find*` exit early.
  - The spec's `TypeError` paths are preserved: an empty `reduce` with no initial value goes to the fallback, which throws.

The only divergence from the spec: a callback that inserts a Proxy into the prototype chain in the middle of the loop sees a different trap order.

**The guard.** `ArrayCbGuard` checks all of the following:

- The receiver is an ordinary `Array`, not a proxy, and has no exotic elements.
- Its prototype is this realm's `%Array.prototype%`, and that object's prototype is `%Object.prototype%`.
- `length` is an own data property.
- The method returned by the single real `Get` is the realm's intrinsic, compared by identity.
- `map` and `filter` only: species is pristine. That means no own `constructor`, `%Array.prototype%.constructor === %Array%`, and `%Array%[@@species]` is the original getter. All three are checked through `SlotCache`s, so the check is cheap.
- `reduce` with no initial value only: the `array_append_unshadowed` protector holds.

If any check fails, the one generic call runs, so behaviour is unchanged.

**Stack traces.**

- `ArrayCbGuard` sets `cur_site = SITE_PC|pc`.
- `stack_trace.rs` looks up `Chunk::inline_regions_at(pc)` and inserts the missing frames. For each open region it adds an arrow frame at the throw position, then `at Array.map (<anonymous>)`.
- `ArrayCbDone` restores the parent's site.
- `Error.stack` and `Error.captureStackTrace` therefore print the same frames as node.

**Tiering.** `ast::scan_expr` flags a function that contains such a call site with `SCAN_HAS_LOOP`, so the function compiles on its first call, the same as a function with a written loop.

**Kill switch.** `LUMEN_INLINE_CB=0` disables the lowering.

### 1.2 Metadata: which native parameters are sync callbacks

Query: `Ctx::sync_callback_params(&callee) -> u32`. Bit `i` set means JS argument `i` is a sync non-escaping callback. For a bare fn pointer, use `sync_callbacks::sync_callback_params_of(NativeFn)`. The query is keyed by native-fn identity, and the mask comes from one of two sources.

**`#[lumen::op]` functions.** A parameter typed `SyncFn<'call>` sets bit `8 + i` of `OpDesc::flags`. The shift is the constant `OP_SYNC_CB_SHIFT`. `SyncFn` has these properties:

- It borrows the argument for the op call's lifetime.
- It is `!Send`, `!Clone`, and not convertible to `Value`.
- Its only method is `call(ctx, this, args)`.

Taken together, these make the non-escape promise checked by the type system. The macro rejects `SyncFn` on `#[op(async)]`.

**Builtins.** `sync_callbacks::BUILTINS` lists the builtin paths. It is resolved to fn pointers once, at the end of `builtins::install`. The list covers:

- `Array.prototype`: `forEach`, `map`, `filter`, `some`, `every`, `find`, `findIndex`, `findLast`, `findLastIndex`, `reduce`, `reduceRight`, `flatMap`, `sort`, `toSorted`.
- The `mapFn` argument of `Array.from`.
- The same methods on `%TypedArray%.prototype`, and the `mapFn` argument of `%TypedArray%.from`.
- The function argument of `String.prototype.replace` and `String.prototype.replaceAll`.
- `Map.prototype.forEach` and `Set.prototype.forEach`.
- The `JSON.parse` reviver and the `JSON.stringify` replacer. Only a *function* passed as the replacer is a callback; an array replacer is data.

## 2. For the JIT: what to build on it

### 2.1 The guard is the contract

Every specialization keys on the same guard as `ArrayCbGuard`.

The first part of the guard is **identity of the method value** read by the one `Get`. Never key on the property name alone. A user can redefine `Array.prototype.map`, and the fallback must then call the user's function.

The second part is **shape of the receiver**: an ordinary array whose prototype is `%Array.prototype%`, whose prototype is `%Object.prototype%`, which rules out a proxy in the chain.

The third part depends on the method:

- `map` and `filter` also need pristine species.
- `reduce` with no initial value also needs the protector.

Hoisting rules:

- The guard may be hoisted out of an *enclosing* loop only while the loop body provably cannot run user code that invalidates it.
- In practice, re-check it at every entry. It costs about 3 `SlotCache` probes.
- Inside the inlined loop, the guard never needs re-checking, because the spec reads `length` once.
- Element reads still go through `GetElem` plus the `HasProperty` fallback, because a callback can make holes or define getters.

### 2.2 Reserved parent slots: frame reuse

The bytecode lowering already allocates the arrow's parameters, its `var`s and the loop state in the parent frame. The loop state is `a`, `len`, `k`, the accumulator `r`, and the extra argument.

- These slots are reserved per call site. They are ordinary locals of the parent, so the register allocator sees them and the loop compiles like a written `for` loop, OSR included.
- For a concise body that never assigns its parameters, the parameters *alias* the loop slots: `k` is the index and `a` is the array.

For a native that the JIT does **not** turn into a loop (`sort`, `replace`, `JSON.parse`, `Map#forEach`), the same idea applies at the machine level:

- **Allocate once.** When a call site passes an arrow literal in a sync-callback slot (`sync_callback_params` on the guarded callee), give the arrow's activation a fixed region of the parent's frame. Do not allocate a new frame per invocation.
- **Reuse the region.** The native re-enters the callback through a thin trampoline that jumps into the same region every time. Because the callback cannot escape, no activation outlives the native call, so the region is reused across invocations.
- **Recursion is safe.** A callback that calls the *same* native again, for example a `sort` inside a comparator, reaches that native through a separate call site and so uses a separate region.
- **Stack traces.** Record the region's owner in the frame metadata so the stack walker can still print `at <arrow> … at Array.sort (<anonymous>)`. §1.1 does the same thing with bytecode PC regions.

### 2.3 Closure scalar replacement

An arrow in a sync-callback slot needs no heap closure object, with two exceptions:

- It reads its own identity. The arrow syntax rules out `arguments.callee`, but `Function.prototype.toString` on the closure is a case the fallback must still handle.
- The native passes the function to user code. None of the marked builtins does this.

Captured variables therefore become loads and stores of the parent's slots: the parent's registers when inlined, or a pointer to the parent frame passed to the trampoline. Captured `let`/`const` in the parent keep their TDZ checks. `this` and `new.target` come from the parent, as they do for any arrow.

The bytecode lowering is the scalar-replaced form already: no `MakeClosure` runs on the fast path. The JIT only has to avoid rematerializing the closure when it inlines or specializes the fallback path.

Two conditions for doing it at a JIT call site:

- **The callee is marked.** The callee at the site must be *guarded* to a native with the parameter's bit set. When the guard fails, materialize the closure and take the generic call.
- **Nested closures are the escape check.** A nested closure inside the arrow that captures the *arrow's* own bindings would need a real environment. The bytecode lowering bails on that case (`CaptureScan`), and the JIT should apply the same rule.

### 2.4 Known costs (measured, 1e6 ints, ns/element)

| method | before (generic call) | inlined (JIT) | node |
|---|---|---|---|
| forEach | ~123 | ~22 | ~15 |
| some / every | ~105 | ~19 | ~11 |
| find / findIndex | ~121 | ~9 | ~11 |
| reduce | ~113 | ~19 | ~12 |
| filter | ~119 | ~73 | ~17 |
| map (lowering now off) | ~106 | ~118 | ~17 |

The remaining cost of `map` and `filter` is `ArrayAppend`:

- The JIT runs it on the generic helper path, at roughly 50 ns natively plus the helper round trip.
- A native lowering would close most of the gap: an in-capacity push of a `Value` into a plain array's dense storage, with the helper as the slow path.
- Presizing the `map` result from `len` would too, as would writing `r[k]` directly, since the result is fresh and non-escaping until the loop ends.

Two more details:

- `ArrayCbHas` only runs when `GetElem` produced `undefined`, so it never runs on dense arrays without holes.
- All three new ops (`ArrayCbGuard`, `ArrayCbHas`, `ArrayCbDone`) are in `generic_ok`. `ArrayCbGuard` and `ArrayCbDone` run once per call and are not worth lowering. `ArrayCbHas` could be lowered as "index < length and the element is not a hole, else call the helper".

Policy note:

- Jit OSR requires an empty operand stack at the loop header. An inlined loop in expression position (`s += a.map(…).length`) has pending operands there.
- `on_backedge` therefore marks such a function `FS_PENDING`, so it gets a whole-function compile at its next entry.
- This is one guarded line in `jit/mod.rs`.

### 2.5 `map` and `filter`: the result append

`map` was briefly excluded from the lowering: its per-element `ArrayAppend` ran through the JIT's generic-helper path (the operand stack copied into a pooled interpreter stack and the op dispatched through `run_vm`), which made the inlined loop slower than the generic call (about 118 vs 106 ns per element).

`ArrayAppend` now has a direct JIT helper (`Helper::ArrayAppend`, a call on the two operand addresses). With it, `map` is about 86 ns per element against about 113 for the generic call, and `filter` about 51 against about 72 (fast profile, 1e6 ints; node: 17 and 17).

The rest of the gap is the general cost of growing and freeing a large array (a plain `r.push(v)` loop is about 150 ns per element against node's 8). Next steps: a presized result for `map` (the result is fresh and does not escape before the loop ends, so `len` capacity can be reserved up front) and a native in-capacity append in the JIT, with the helper as the slow path.
