# Async functions without promise plumbing — lessons from .NET runtime-async

Status: design proposal (no code yet). Sources: dotnet/runtime at `5b82fc4` (2026-09-23), lumen
working tree on `perf/gc-handle` (2026-09-24). Paths prefixed `rt:` are relative to the
dotnet/runtime root; bare paths are relative to `crates/lumen/src/`.

## 1. What .NET runtime-async is

Until .NET 10, `async` in C# was a Roslyn rewrite: every async method became a struct state
machine plus an `AsyncTaskMethodBuilder`; the first real suspension boxed the struct into a heap
`AsyncStateMachineBox` holding *every* hoisted local, and every level of an await chain returned
its own `Task`. Runtime-async (`MethodImplOptions.Async`, IL flag `0x2000`,
`rt:docs/design/specs/runtime-async.md:14,86`) moves suspension into the JIT/VM: the compiler
emits plain IL with `call Foo; call AsyncHelpers.Await(Task)` pairs, and the JIT turns each such
pair into a suspension point (`rt:docs/design/specs/runtime-async.md:65`). Only locals live
across a suspension are "hoisted" (`:67`), and suspension is optional: "suspension is not
required if all Task-like objects are completed" (`:28`). A VM-based variant was prototyped in
.NET 9 and dropped in favour of the JIT one ("at least as good as compiler-async in all the
configurations that we measured", dotnet/runtime#94620).

### 1.1 Calling convention

`rt:docs/design/coreclr/botr/runtime-async-codegen.md:166-204`:

* Every async-callable method (the *async variant*) takes one extra hidden argument, the
  `Continuation` — `null` on a normal call, the saved continuation when resuming — and returns
  one extra hidden value: `null` = completed synchronously (the ordinary return register holds
  the result), non-null = "I suspended; here is the head of the continuation chain".
* After every async call the JIT inserts `if (returnedContinuation != null) goto suspend;`
  with branch likelihood 0 for the suspend edge (`rt:src/coreclr/jit/async.cpp:3829-3913`,
  `CreateCheckAndSuspendAfterCall`; `setLikelihood(0)` at `:3899`). The completed path is
  ordinary straight-line code.
* Non-async code reads the hidden return through the `AsyncHelpers.AsyncCallContinuation()`
  intrinsic (`rt:src/coreclr/System.Private.CoreLib/src/System/Runtime/CompilerServices/AsyncHelpers.CoreCLR.cs:206`);
  the leaf that really blocks calls the `AsyncSuspend(Continuation)` intrinsic (`:198`), which
  returns that continuation as the hidden value.

### 1.2 Fast path: awaiting something already complete

`AsyncHelpers.Await(Task)` (`rt:src/libraries/System.Private.CoreLib/src/System/Runtime/CompilerServices/AsyncHelpers.cs:148-158`)
is itself an async method: `if (!task.IsCompleted) { TailAwait(); Suspend(task, …); return; }
TaskAwaiter.ValidateEnd(task);`. When the awaited call ran to completion no `Task` is created
(the task-returning form is never used between two runtime-async methods), no continuation is
allocated, no state is copied: the cost of the await is one compare-and-branch on the hidden
return register. Reported numbers: sync completion through a 12-deep await chain 52.5 → 18.3 ns
(~4.4 → ~1.5 ns per level) and 0 B/op (fluxzy.io .NET 11 write-up; Kai Sawano's Medium
benchmarks report the same "0 bytes, no Gen0" for no-suspension chains).

### 1.3 Suspension: materializing only live state

`AsyncTransformation::Run` (`rt:src/coreclr/jit/async.cpp:1119-1330`) runs just before
lowering, on LIR, after a dedicated async liveness pass (`fgAsyncLiveness`, `:1184`):

* **Live set per await.** `CreateLiveSetForSuspension` (`:1590-1672`) takes the locals live
  after the call, minus the call's own definition (it is overwritten on resume unless live into
  an EH successor, `:1600-1620`), minus locals only used on the synchronous path (saved
  contexts), plus LIR temporaries live across the call (`LiftLIREdges`, `:1705`) — i.e. the
  evaluation stack is spilled too.
* **Default/preserved-value analyses** (`rt:src/coreclr/jit/asyncanalysis.cpp`): a local still
  at its zero default needs no save; a local unchanged since the last resumption need not be
  re-saved when the continuation is reused.
* **Layout.** `ContinuationLayoutBuilder::Create` (`:1998-2120`) sorts GC refs first, then by
  alignment (`:2034-2052`), then appends optional well-known members — OSR address (must be at
  offset 0), `ExecutionContext`, continuation context (SynchronizationContext/TaskScheduler),
  exception slot (only if the await is inside a `try`), and one result slot per callee return
  type. Their positions are encoded as 2–21-bit indices in `Continuation.Flags`
  (`AsyncHelpers.CoreCLR.cs:22-54`). The VM creates a `Continuation` subclass per
  `(size, GC-ref bitmap)` and shares it across methods
  (`rt:src/coreclr/vm/asynccontinuations.cpp:191`, `jitinterface.cpp:10712`). With more than one
  await, all suspension points share one union layout (`CreateResumptionsAndSuspensions`,
  `:4879-4990`), so one continuation object serves the whole method.
* **Continuation object.** `class Continuation { Continuation? Next; ResumeInfo* ResumeInfo;
  ContinuationFlags Flags; int State; /* data */ }` (`AsyncHelpers.CoreCLR.cs:76-92`);
  `ResumeInfo` holds the resume function pointer and a diagnostic IP (`:57-74`).
* **Suspension block** (`CreateSuspension`, `:2725-2898`): allocate (helper
  `CORINFO_HELP_ALLOC_CONTINUATION(prev, type)` which also links `prev.Next = new`,
  `AllocContinuation` at `AsyncHelpers.CoreCLR.cs:332-341`), store `ResumeInfo`, `State` (the
  await's number), `Flags`, then the live locals (`FillInDataOnSuspension`, `:3030-3118`), then
  capture contexts and return the continuation.
* **Continuation reuse.** If this await is reachable after a previous resumption, the incoming
  continuation (the hidden argument) is reused instead of allocating: the suspension block
  branches on `reuse != null` (`:2750-2800`, reuse var at `:2781`), and only locals mutated since the last resumption
  are re-stored (`SaveSet::MutatedLocals`). A loop of N real suspensions allocates one
  continuation, not N.
* **Resumption.** A new entry block tests the hidden argument; non-null jumps to a switch on
  `Continuation.State` (or a single branch / compare for 1–2 states), each target restores the
  saved locals (`RestoreFromDataOnResumption`, `:3995`), rethrows a stored exception
  (`RethrowExceptionOnResumption`, `:4129`), copies the awaited result out of its result slot
  (`CopyReturnValueOnResumption`, `:4218`) and falls into the code after the await
  (`CreateResumptionSwitch`, `:5088-5280`). The resume entry is a tiny IL stub
  (`rt:src/coreclr/tools/Common/TypeSystem/IL/Stubs/AsyncResumptionStub.cs`) that calls the
  method with default arguments and the continuation set via `SetNextCallAsyncContinuation`, and
  writes the result into the caller's result slot.

### 1.4 Chains: unwind by allocating bottom-up, resume by walking `Next`

On suspension each frame, innermost first, returns its continuation to its caller, which
allocates its own continuation, links it (`prev.Next = mine`) and returns too — only frames that
are actually on the stack when a real suspension happens pay for a continuation. The first
awaiter (`Suspend(Task)`, `AsyncHelpers.CoreCLR.cs:370-404`) hangs a `RuntimeAsyncTaskContinuation`
off a per-thread sentinel; the task-returning thunk at the top of the chain wraps the list in a
single `RuntimeAsyncTask` (`CreateRuntimeAsyncTask`, `:1308`) and subscribes it once
(`HandleSuspended`, `:772-900`). **One Task per chain, not per frame.**

Resumption is `RuntimeAsyncTask.DispatchContinuations` (`:919-1045`): a loop that pops the head,
restores its `ExecutionContext`, calls `ResumeInfo->Resume(cont, ref resultLoc)` where
`resultLoc` is the *next* continuation's result slot (`:963`) — so a callee returns its value
straight into its caller's saved frame — and continues with `Next` until the list is empty (then
completes the task) or a resumed frame suspends again (its new continuation is spliced in front
of the remainder, `:967-970`). Exceptions: `UnwindToPossibleHandler` (`:1171-1190`) skips
frames whose continuation has no exception slot (the await was not in a `try`), appending each
frame's diagnostic IP to the stack trace, and resumes the first that can catch.

A **tail await** (`return await X` with nothing after, `TailAwait()` intrinsic, `:210`;
`TransformTailAwaits`, `async.cpp:1400`) creates no suspension point at all — the frame just
forwards the callee's continuation.

### 1.5 Interop both ways

Each async method has two entry points (`rt:src/coreclr/vm/asyncthunks.cpp:14-62`): the async
variant with the hidden-continuation ABI, and a Task-returning thunk (`EmitTaskReturningThunk`,
`:77-106`) for ordinary callers: call the variant; if `AsyncCallContinuation() == null` return
`Task.FromResult(result)` (cached for common values), else `CreateRuntimeAsyncTask`; exceptions →
`TaskFromException`. In the other direction an old state-machine method (or any Task-returning
method) gets an async variant that calls it and `TransparentAwait`s the returned task
(`AsyncHelpers.CoreCLR.cs:633-730`) — zero cost when the task is already complete, a
`TailAwait` into `TransparentSuspend` otherwise. Measured caveat: mixing (BCL on runtime-async,
user code on state machines) *regressed* a ping-pong benchmark (1.42 → 1.18 M msg/s, +30%
allocations) because every boundary crossing materializes a Task (fluxzy.io).

### 1.6 Contexts and exceptions

`rt:docs/design/coreclr/jit/runtime-async-inlining.md:38-66`: each async body is wrapped in
capture/restore of `ExecutionContext` + `SynchronizationContext`, restored only when the method
finishes *synchronously*; a suspension captures the `ExecutionContext` into the continuation and
the dispatcher restores it before resuming (`DispatchContinuations`, `:958-961`). The continuation
context (where to resume) is only captured for awaits that need it; with
`ConfigureAwait(false)` a flag bit replaces it. `QueueContinuationFollowUpActionIfNecessary`
(`:1192-1250`) decides per frame whether to keep running inline or post to the captured
context. Contexts, exception slot and result slots are all *optional members*, present only when
the await needs them.

### 1.7 Tiering / OSR

Tier-0 code has patchpoints; if the method is later on-stack-replaced, the OSR body can
suspend with a continuation that must resume in the OSR code, so the first pointer-sized data
member is an "OSR address" (`FillInDataOnSuspension`, `:3036-3055`). The tier-0 resumption
switch first checks it and jumps (`GT_NONLOCAL_JMP`) into the OSR method (`CreateOSRJumpBB`,
`:5064`; `:5200-5240`); an OSR method ignores a tier-0 continuation it did not create
(`:5242-5280`) and never reuses it. Inlining an async callee needs per-inlinee "resumed" flags
and context bookkeeping (`runtime-async-inlining.md:68+`) — the hardest part of the design.

### 1.8 Measured gains (third-party, .NET 11 previews)

| scenario | state machine | runtime-async |
|---|---|---|
| sync completion, 12-deep chain | 52.5 ns/op | 18.3 ns/op, 0 B (2.9×) |
| suspension ping-pong (channels) | 1.42 M msg/s, 3120 B/msg | 3.16 M msg/s, 1456 B/msg |
| deep state-machine chain w/ suspension | — | 7.4× (Sawano) |
| ThreadPool / TCS continuations | — | 3–4× (Sawano) |
| end-to-end HTTP proxy | — | +8…13% throughput |

The lesson: the big micro-wins come from (a) not allocating anything on synchronous completion,
(b) one continuation per *suspended frame* and one Task per *chain*, and (c) resuming a whole
chain in one dispatcher loop. Real applications see ~10%.

## 2. lumen today

### 2.1 Execution

* An async call goes through the full tree-walker call setup — `!func.is_async` excludes it
  from the lean compiled path (`interpreter.rs:5546-5549`), so it allocates an activation `Env`,
  binds params into its string-keyed map and inserts `this` (`interpreter.rs:5650-5780`), then
  `run_async` (`interpreter.rs:6025`) builds a `VmCoro` (`bytecode.rs:6543,6576`) that *again*
  seeds slots from `args` and calls `make_run_env`. `VmCoro::new` allocates fresh `slots`,
  `stack` (capacity 16) and `handlers` vectors — it does not use `vm_pool`, unlike `run`
  (`bytecode.rs:4613`).
* `run_async` creates the result promise (`new_promise`, `eval.rs:3366`: `Object::new`, an
  `extra_protos.get("Promise")` string lookup, a weak-pin push, a `promises` side-table insert),
  inserts the coroutine into `Interp::generators` keyed by the promise address
  (`interpreter.rs:851`), and calls `drive_async` (`interpreter.rs:6140`), which removes it,
  resumes it, and re-inserts it on `await` — two hash operations per step.
* The body runs on `drive_vm` (`bytecode.rs:4667`) as a root frame; `Op::Await` returns
  `VmStep::Await(v)` (`bytecode.rs:6414`). Async callees never run as inline frames
  (`inline_callee` excludes `is_async`, `bytecode.rs:4847`; `debug_assert!(… "only the root
  frame can await")` at `:4720`), and the whole-function JIT refuses any chunk containing
  `Op::Await` (`bytecode/jit/build.rs:671`) and any async callee (`bytecode/jit/call.rs:541`).
  Async bodies are therefore always interpreted.
* Bodies that do not compile fall back to OS-thread coroutines (`coroutine.rs`, ~µs per
  handoff). Any `CaptureScan` bail (e.g. a closure capturing a per-iteration `let`) puts the
  *whole* async function there.

### 2.2 Await and promises

`await_subscribe` (`eval/promise_fast.rs:123`) is already allocation-light: a non-object is
queued as a `Job` directly; an object goes through `promise_resolve_checked`
(`interpreter.rs:6337`: a full `get_member(v, "constructor")` for a native promise, else a new
promise + `resolve_promise`) and then `promise_then_into` (`eval.rs:3505`) with the async
function's own promise as handler and `Value::Empty` as the result marker, so resumption needs
no closure pair (`run_await_job`, `promise_fast.rs:147`). Completion calls `resolve_promise`
(`eval.rs:3401`), which does the `promise_forward` lookup, a `then` lookup for objects, and
`settle` (`eval.rs:3465`) which moves each reaction into a `Job` (`interpreter.rs:948`; five
`Value`s, 80+ bytes). `drain_microtasks` (`eval.rs:3600`) pops jobs from a `VecDeque`.

### 2.3 Where the time goes

Measured with a fresh release build of the current tree (script in §7; Windows x64, per
operation, lower is better; Node 22 for reference):

| benchmark (per op) | lumen (bytecode tier) | Node 22 |
|---|---|---|
| sync call in a loop (`s += leafSync(i)`, JIT) | 3 ns | 3 ns |
| `it.next()` on a compiled generator | 260–280 ns | 50 ns |
| `leaf(i)` — async call, result dropped, no await inside | 780–860 ns | 20 ns |
| `await i` (non-promise) in an async loop | 110–120 ns | 90 ns |
| `await p` (settled native promise) | 180–190 ns | 70 ns |
| `await leaf(i)` (callee returns without suspending) | 1230–1300 ns | 100 ns |
| `await top(i)`, 3-deep `return await` chain | 3400–3470 ns | 290 ns |

The await tick itself is fine (`await i` is within 1.3× of V8). **The cost is the async call**:
~800 ns to create an async activation that never suspends, i.e. ~85% of `await leaf()` and of
each chain level. (An older binary without the whole-function JIT measured the same shapes at
2–8 µs; and any async body that bails `CaptureScan` runs on thread coroutines at ~4 µs/await.)

Per level of an `await f()` chain lumen pays: an activation `Env`, a `VmCoro` with three `Vec`s,
a promise object + side-table entry + weak pin, two `generators` hash operations per step, a
`constructor` property lookup, a reaction tuple, a `Job`, and the `resolve_promise` walk — and
then the whole thing again for the caller. None of it is needed when the promise never escapes.

## 3. JS constraints that differ from .NET

1. **Every `await` takes at least one microtask tick**, even on a settled value (ES2019 reduced
   native-promise awaits from 3 ticks to 1, but never to 0). The ordering of that tick relative
   to other queued jobs is observable. So lumen's "fast path" cannot skip the tick; it can only
   make the tick cheap: no promise, no reaction record, no closure, no frame copy.
2. **One tick per chain level.** When a callee completes after suspending, its caller resumes
   in a *separate* job appended to the queue tail — not in the same dispatcher iteration as .NET
   does. A 3-deep chain costs 3 ticks. Direct continuation chaining saves the objects, not the
   ticks.
3. **The callee's promise is a real object** whenever it can be observed (`f().then`, stored,
   `Promise.all`, identity checks). It can only be elided when the call's result flows directly
   into `await` (`Call; Await` in bytecode) — the JS analogue of .NET's recognized
   `call M; call Await` IL pair.
4. **PromiseResolve is observable**: `await p` reads `p.constructor`
   (`promise_resolve_checked`); `return v` from an async function reads `v.then` if `v` is an
   object, and a thenable adds two ticks (NewPromiseResolveThenableJob). Both must stay exactly
   where the spec puts them.
5. **Unhandled-rejection tracking** observes a rejected promise with no handler; an elided
   promise that is always awaited is handled by construction, but anything that is not
   `Call; Await` must keep a real promise.
6. **Sync prologue**: an async function runs synchronously until its first `await`; a throw
   before that rejects the returned promise (does not throw to the caller).

## 4. Proposed design

### 4.1 Async frames live on the VM stack until the first real suspension

Allow an async callee to run as an `InlineFrame` of the caller's `drive_vm` (and a root call to
use pooled buffers like `run` does). Add `InlineFrame::kind: Sync | Async { link: AwaitLink }`
where `AwaitLink` says who consumes the result:

* `Link::Promise` — the call was not followed by `Await`: the result must be a promise
  (materialized lazily, see 4.3).
* `Link::Parent` — the call site is `Call; Await` in an async caller: the result is delivered
  straight to the caller's await.

On `Op::Await` inside an async inline frame (today a debug assertion), **spill**: move the frame
record's `slots`/`stack`/`handlers`/`chunk`/`env`/`this` into a new `VmCoro` (a `Vec` move is
three words; the frame record gets fresh pooled buffers) — no value is copied. Pop the frame and
continue in the caller:

* `Link::Promise`: materialize the result promise now, store the `VmCoro` keyed by it, subscribe
  as today, push the promise on the caller's stack.
* `Link::Parent`: store `parent = caller` in the new `VmCoro`, then make the caller suspend at
  its `Await` without subscribing to anything ("awaiting a linked child"). If the caller is
  itself an inline async frame it spills the same way, recursively — the .NET bottom-up
  unwind (§1.4): only frames on the stack at a real suspension get heap state.

If the async frame instead *returns* (no suspension), its result goes to its consumer without
any promise:

* `Link::Parent` + non-object result (or an object whose `then` is not callable — the `Get`
  happens here, at the spec's point): the caller's `Await` sees a *pre-settled* value and parks
  with a direct `Job` (the non-object branch of `await_subscribe` already does exactly this).
  Allocation: one `Job` in the ring buffer.
* throw → same, with `fulfilled: false`.
* thenable result, or `Link::Promise`: create the promise and run the existing
  `resolve_promise`/`reject_promise` — identical observable behaviour.

The compiler marks `Call; Await` sites (a flag on the `Call` op, or a fused `CallAwait(argc)`
op) so the frame knows its link kind at push time. The `constructor` read that `await P` would
do on the elided `P` hits `%Promise.prototype%.constructor`; the fast path is only legal while a
realm flag `promise_proto_pristine` is set (cleared by any define/set of `constructor` or
`then` on `%Promise.prototype%`, and by `Symbol.species` tampering is irrelevant here).

### 4.2 Direct continuation chaining after suspension

A spilled `VmCoro` with `parent` set completes by enqueuing `Job::Resume { coro: parent, value,
fulfilled, context }` instead of settling a promise — the same queue position as settling `P`
with one reaction (spec: TriggerPromiseReactions appends one job). A thenable completion value
falls back to materializing `P`, resolving it, and attaching `parent` as its only reaction.
`context` is the `async_context` captured when the parent parked (what
`promise_then_into` records today). Chains become a parent-linked list of `VmCoro`s: the
promise-free analogue of .NET's `Continuation.Next`.

Resumption stays one job per level (constraint 2). Two cheap refinements:

* `run_await_job` currently re-enters through `drive_async` → `generators.remove/insert`. Store
  suspended coroutines in a slab (`Vec<Option<Box<VmCoro>>>` + free list) and put the slab index
  in the job; the promise-keyed map stays only for the thread-coroutine fallback and for
  promise-linked frames.
* When the resumed frame completes and its parent job is the only job in the queue, running it
  immediately is indistinguishable from enqueue-then-pop; do it in the drain loop (a trampoline,
  not recursion) to skip the `VecDeque` round-trip.

### 4.3 Awaiting native promises

* Replace the `get_member(v, "constructor")` in `promise_resolve_checked` with a pristine check:
  `v`'s `[[Prototype]]` is the realm's `%Promise.prototype%`, `v` has no own `constructor`, and
  `promise_proto_pristine` — then skip the lookup (it would return `%Promise%`).
* Reactions: `PromiseState::reactions` holds `(Value, Value, Value, Value)` (64 bytes) and
  `settle` clones into `Job`s. Add a compact `Reaction::Await { coro: u32, context }` variant so
  an await subscription stores a slab index, not the async function's promise object — which
  also means a `Link::Parent` coroutine never needs a promise at all, even when it awaits a real
  pending promise.
* Cache the `%Promise.prototype%` handle on `Interp` instead of `extra_protos.get("Promise")` in
  `new_promise`.

### 4.4 Live state at suspension

A `VmCoro` today keeps all `n_slots` slots plus the operand stack across a suspension. Copying
is not the cost (the spill is a move); retention is: dead slots keep objects alive across long
awaits. Borrow .NET's liveness idea cheaply: the compiler already has per-op stack analysis for
the JIT (`jit_ir.rs:1029` `analyze_stack`); compute a per-`Await` live-slot bitset once per chunk
and, at spill time, overwrite dead slots with `Undefined`. A compacted continuation (only live
slots, GC-refs first, .NET §1.3) pays off only once native code owns the frame (4.6).

Note .NET's continuation *reuse* is free in lumen: a `VmCoro` persists across all its awaits.

### 4.5 Contexts and exceptions

* `async_context` (the AsyncContext / AsyncLocalStorage value) plays the role of
  `ExecutionContext`: capture it into the parent link / await reaction at park time (as
  `promise_then_into` does now) and install it around the resume (as `run_await_job` does). No
  capture on synchronous completion — the .NET rule.
* Exceptions: a rejected await resumes with `pending_throw` (`VmCoro::resume` already injects it
  at the suspension so `try`/`catch` works). An uncaught throw from a `Link::Parent` coroutine
  enqueues a rejected resume of the parent; the parent's handlers run in its own tick. No
  `UnwindToPossibleHandler`-style skipping (every level needs its tick anyway).
* Async stack traces come nearly free: at `Error` capture, walk the running coroutine's
  `parent` links and append each parent's function name / pc — what .NET does with
  `DiagnosticIP` (`AsyncHelpers.CoreCLR.cs:1175-1182`) and V8 does by walking promise reactions.

### 4.6 JIT

1. **Await as an exit.** Let `analyze` accept `Op::Await` in function mode as a terminator that
   exits: native code writes back SSA locals to `slots`, materializes the operand stack, and
   returns a new `EXIT_AWAIT` kind (the existing exit-word contract, `bytecode/jit/mod.rs`
   header). `on_entry` maps it to `VmStep::Await`. Only the part up to the first await runs
   native; the rest resumes in the interpreter. This alone makes the synchronous prologue and
   await-free async functions (common: `async` wrappers that return a value) JIT-able.
2. **Resume entries.** Give function code a second entry that takes a state number (the await
   index) and dispatches through a switch to per-await resume blocks that reload SSA locals from
   `slots` and the operand stack from the frame — `CreateResumptionSwitch` (§1.3). The
   per-await live-slot bitset from 4.4 tells which slots to reload and which kinds to guard.
3. **Loops with awaits.** Loop OSR currently fails for any loop containing `Await`
   (`build.rs:671`); with (1) and (2) the region can exit at the await and re-enter at its resume
   block, instead of the loop running interpreted forever.
4. **OSR interaction** mirrors .NET §1.7: a `VmCoro` suspended by native code records which code
   version and state it belongs to; on resume, if the code was invalidated (deopt/recompile),
   resume in the interpreter at the saved pc — always possible because suspension already wrote
   the full interpreter frame. This is simpler than .NET's OSR-address slot because lumen's
   continuation *is* the interpreter frame.

## 5. Expected gains

Estimates for the `Call; Await` of an async function that returns without suspending (the
dominant pattern in real code: caching layers, validation wrappers, `async` functions whose awaits
hit settled values):

| cost removed | where |
|---|---|
| activation `Env` + param/`this` map inserts | Stage 0 (lean entry) |
| `VmCoro` + 3 `Vec` allocations, slot seeding twice | 4.1 (inline frame, pooled buffers) |
| promise object, side-table entry, weak pin | 4.1 (`Link::Parent`) |
| `generators` insert/remove ×2 | 4.1/4.2 |
| `constructor` lookup, reaction tuple, `resolve_promise` | 4.1/4.3 |

What remains per level: a frame push/pop on the caller's loop (a compiled sync call costs a few
ns), one `Job` push/pop and one `drive_vm` re-entry — about the cost of `await i` today
(~120 ns). Against §2.3:

* `await leaf(i)`: ~1250 ns → ~150–250 ns (**5–8×**), allocation from ~8 heap objects/vectors
  to 0 (the `Job` lives in the `VecDeque` ring).
* 3-deep chain: ~3400 ns → ~450–700 ns (**5–7×**): each level still needs its own tick, but a
  tick is ~120 ns, not ~1100.
* Stage 0 alone (lean entry + pooled `VmCoro` buffers, no semantic change) should remove a large
  part of the ~800 ns call cost; estimate 1.5–2.5× on `await leaf()`.
* Suspending chains (awaiting real I/O) gain less, ~1.5–2×: they keep one `VmCoro` per
  suspended level, but lose the promise, side-table and hash traffic per level.
* `await p` on a settled native promise: ~185 → ~130 ns from the pristine `constructor` check.

JIT'ing async bodies (4.6) is multiplicative on the code *between* awaits (today async bodies
are interpreted even when the same code in a sync function runs ~20–50× faster native) and is
what makes CPU-heavy async code (parsers, stream transforms, `for await` pipelines) approach
sync speed. For comparison, .NET's end-to-end gain from the same ideas was ~10% on a real proxy;
node-style workloads in lumen are further from the floor, so the end-to-end upside is larger.

## 6. Risks

* **Microtask ordering.** Every change must keep job *count* and *position* identical. The
  invariant to test: for each await, exactly one job is enqueued at the moment the spec's
  PerformPromiseThen/TriggerPromiseReactions would enqueue it. Thenable results (two extra ticks)
  and non-native promises always take the old path. Build a differential test that logs
  interleavings of `Promise.resolve().then` chains against async chains under both paths
  (test262 `built-ins/AsyncFunction`, `language/expressions/await` cover much of it).
* **Promise identity** is only elided at `Call; Await`; anything else (including
  `await (0, f())`? — still `Call; Await` after the comma folds; `await f?.()`, `await
  super.m()`) must be checked at the bytecode level, not by syntax.
* **`then`/`constructor` lookups on user-modified `Promise.prototype`** — gated by the pristine
  flag; subclass promises and cross-realm promises take the slow path.
* **Re-entrancy while spilled frames are half-built** — spilling happens inside `drive_vm`'s
  loop; getters (`then` lookup on a returned object) can run user code. Do the `then` `Get`
  before popping the frame, or after the spill is complete, never in between.
* **Generators and async generators** share `VmCoro`; the link machinery is async-function-only
  at first. Async generators keep the promise-per-`next()` path.
* **Thread-coroutine fallback** (`coroutine.rs`) cannot be inlined or linked; a `Link::Parent`
  call to a non-compiled async function must materialize the promise. Reducing `CaptureScan`
  bails matters more than anything here: one unanalyzable closure turns a whole async function
  into µs-per-await thread handoffs.
* **GC.** Parent links and slab entries are strong edges the cycle collector must trace; a
  never-resumed chain (awaiting a forever-pending promise) must be collectable when the promise
  dies (today: `promise_weak_prune` drops coroutines keyed by dead promises,
  `promise_fast.rs:46-86`) — the slab needs the same hook, keyed by what the coroutine waits on.
* **Debuggability / stack traces** change shape (frames that used to have a promise no longer
  do); cover `Error.stack` expectations in node-compat.

## 7. Staged plan

1. **Stage 0 — lean async entry** (small, independent): route compiled async functions through
   the lean path (`bind_compiled_this`, slot seeding, no activation `Env`), take `VmCoro`
   buffers from `vm_pool` and return them in `release`, cache `%Promise.prototype%`, pristine
   `constructor` check in `promise_resolve_checked`. Re-measure.
2. **Stage 1 — slab + compact await reactions**: coroutines in a slab, `Job`/reaction variants
   that carry a slab index; `generators` map only for thread coroutines and generators.
3. **Stage 2 — `Call; Await` fusion with sync-completion fast path**: async callees as inline
   frames with `Link::Parent`; no promise when the callee returns or throws synchronously.
4. **Stage 3 — spill on first suspension + parent chaining**: bottom-up spill of inline async
   frames, parent-linked completion, lazy promise for `Link::Promise`; per-await dead-slot
   clearing.
5. **Stage 4 — JIT**: `EXIT_AWAIT`, then resume entries with a state switch, then loops that
   contain awaits.
6. **Stage 5 — diagnostics**: async stack traces from parent links.

Each stage ships behind the existing tier tests (`Tier::Interp` vs `Tier::Bytecode` oracle
comparisons in `bytecode/generator.rs` style), test262 async suites, and the micro-benchmark
below.

### Benchmark script

```js
(function(){
function leafSync(x){ return x + 1; }
function syncLoop(n){ let s=0; for (let i=0;i<n;i++) s+=leafSync(i); return s; }
function* g(){ let i = 0; while (true) yield i++; }
function genLoop(n){ const it = g(); let s = 0; for (let i = 0; i < n; i++) s += it.next().value; return s; }
async function leaf(x) { return x + 1; }
async function mid(x) { return await leaf(x); }
async function top(x) { return await mid(x); }
async function awNum(n){ let s = 0; for (let i = 0; i < n; i++) s += await i; return s; }
async function awRes(n){ let s = 0; const p = Promise.resolve(1); for (let i = 0; i < n; i++) s += await p; return s; }
async function awLeaf(n){ let s = 0; for (let i = 0; i < n; i++) s += await leaf(i); return s; }
async function awChain(n){ let s = 0; for (let i = 0; i < n; i++) s += await top(i); return s; }
function callOnly(n){ for (let i = 0; i < n; i++) leaf(i); }
const N = 100000, out = {};
let t = Date.now(); syncLoop(N*10); out.syncCall = (Date.now()-t)*1e6/(N*10);
t = Date.now(); genLoop(N); out.genNext = (Date.now()-t)*1e6/N;
t = Date.now(); callOnly(N); out.asyncCallNoAwait = (Date.now()-t)*1e6/N;
const steps = [["awaitNum", awNum], ["awaitResolved", awRes], ["awaitLeafCall", awLeaf], ["await3Chain", awChain]];
let k = 0;
function next(){
  if (k === steps.length) { for (const key in out) out[key] = Math.round(out[key]); console.log(JSON.stringify(out)); return; }
  const [name, fn] = steps[k++]; const t0 = Date.now();
  fn(N).then(function(){ out[name] = (Date.now()-t0)*1e6/N; next(); });
}
next();
})();
```

## References

* `rt:docs/design/specs/runtime-async.md`, `rt:docs/design/coreclr/botr/runtime-async-codegen.md`,
  `rt:docs/design/coreclr/jit/runtime-async-inlining.md`
* `rt:src/coreclr/jit/async.cpp`, `async.h`, `asyncanalysis.cpp`
* `rt:src/coreclr/System.Private.CoreLib/src/System/Runtime/CompilerServices/AsyncHelpers.CoreCLR.cs`,
  `RuntimeAsyncTaskContinuation.cs`; `rt:src/libraries/System.Private.CoreLib/src/System/Runtime/CompilerServices/AsyncHelpers.cs`
* `rt:src/coreclr/vm/asyncthunks.cpp`, `asynccontinuations.cpp`;
  `rt:src/coreclr/tools/Common/TypeSystem/IL/Stubs/AsyncThunks.cs`, `AsyncResumptionStub.cs`
* dotnet/runtime#94620 (.NET 9 runtime-async experiment conclusion)
* https://www.fluxzy.io/resources/blogs/net11-runtime-async-real-codebase
* https://medium.com/@skyake/how-fast-is-net-11-runtime-async-b9c821529cd5
