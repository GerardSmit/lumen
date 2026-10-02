# lumen

Bitnest embedding changes in this checkout include the native ARM64 JIT memory
backend, soft-float helper ABI, collection API and a timer-only installation
without the hosted filesystem/thread substrate. Review these changes manually
before committing or updating the parent submodule pin.

A from-scratch JavaScript **engine** in Rust — std only, zero dependencies — and a
**runtime** being built on top of it, the way Node/Deno/Bun wrap a JS engine with an event
loop and host APIs. The engine remains std-only. The Node runtime uses `libc` on Unix for portable kernel
file-descriptor interoperability; it does not use `tokio`, `mio`, `rustyline` or `serde`.

## The engine (`crates/lumen`)

A lexer, parser, and **two execution tiers**:

- a **tree-walking interpreter** — the reference oracle: the spec semantics live here, and
  the other tier must match it observably (a differential fuzzer, `lumen-difftest`, holds
  them to that);
- a **bytecode VM** (the default) — functions compile whole (or not at all — no
  deoptimization) to a stack machine with slot-homed locals, per-site inline caches for
  property and free-name access backed by object shapes (hidden classes), and dense-array
  element fast paths.

A new optimizing native compiler is being built to replace the earlier template JIT; see
[docs/jit.md](docs/jit.md).

Tier selection: `--tier=interp|bytecode` (**bytecode is the default**; `jit` is still
accepted as an alias for it). Functions tier up after a call-count threshold — immediately if
the body contains a loop. Force the reference tree-walker with `--tier=interp` (or
`LUMEN_TIER=interp`).

The language surface: generators and `async`/`await` running on stackful coroutines (async
bodies suspend on the bytecode VM itself), full `RegExp` (including `\p{…}` and inline
modifiers), typed arrays, `Proxy`/`Reflect`, ES modules (top-level await, `import defer`,
source phase), `Intl`, and `Temporal`.

`Intl` (ECMA-402) and its CLDR data tables are behind the default-on `intl` cargo feature —
the largest single contributor to binary size (~3 MB of the release binary). Build with
`--no-default-features` for a small engine: the `Intl` global is absent and the `toLocale*`
methods degrade to their locale-independent forms, the way engines built without i18n do.

On dependencies and `unsafe`: the engine stays std-only; the Node runtime has a narrow Unix
`libc` dependency for descriptor ABI constants and operations. `unsafe` is concentrated in the
object heap and its inline-cache fast paths, and in the N-API addon loader's `dlopen` bridge.

**Passes 100% of [tc39/test262](https://github.com/tc39/test262): 53,577/53,577** (including
annexB, intl402, and staging) — on the compiled tier and under `LUMEN_TIER=interp`. One
test is skipped: `annexB/.../block-decl-func-skip-arguments.js` predates the current
FunctionDeclarationInstantiation text and contradicts two SpiderMonkey staging tests (and V8).

Extracted from — and used by — the [lucid-softworks/browser](https://github.com/lucid-softworks/browser)
engine as its JS backend (`backend-lumen`), with full git history.

## The runtime

A curated `embed` API on the engine exposes just enough — native-function registration, a
typed host-state slot, and event-loop hooks — for a runtime layer to be assembled from
independent op crates, without leaking the interpreter's internals into the published API.
On top of that:

- **Event loop** (`lumen-runtime`) — a single loop thread owns the (`!Send`) engine; blocking
  work runs on a std thread pool and completes back over `mpsc`. No epoll/kqueue reactor
  (that would need raw syscalls); the thread-pool-plus-completion model is libuv's own fs
  strategy. Each turn drains microtasks, queued callbacks, due timers, and I/O completions,
  then blocks until the next event. Native task registration captures the admitting immutable
  async-context frame and restores it for decoder/callback delivery, so raw I/O callbacks
  retain AsyncLocalStorage state just as promise continuations do.
- **Timers** (`lumen-timers`) — `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/
  `setImmediate`, plus `queueMicrotask`.
- **`console` and `process`** — streaming `console.*`; `process.argv`/`env`/`platform`/
  `cwd()`/`exit()`/`nextTick()`.
- **Filesystem** — `node:fs` (sync, callback and promise APIs, the async forms on the thread
  pool) over `lumen_os::vfs`, the file-system interface the Python runtime shares: the OS
  natively, an in-memory tree on wasm.
- **Web platform** (`lumen-web`) — a growing slice of the WinterTC Minimum Common API:
  `Event`/`EventTarget`/`CustomEvent`/`AbortController`/`AbortSignal`/`DOMException`,
  `TextEncoder`/`TextDecoder` (UTF-8 and WHATWG Windows-1252 labels), `atob`/`btoa`, `structuredClone`, `URL`/`URLSearchParams`,
  `performance.now()`, `crypto.getRandomValues`/`randomUUID`/`subtle.digest` (SHA-256), and
  `fetch`/`Headers`/`Request`/`Response`. See the checklist at the top of
  `crates/lumen-web/src/lib.rs` for what's implemented vs. deferred (streams, `Blob`/
  `FormData`, `URLPattern`, …).

  `fetch` speaks HTTP/1.1 over `std::net`. **`https:` is not supported**: TLS cannot be
  implemented on std alone and no third-party crate is permitted, so `https` URLs reject with
  a clear error; plain `http` works.

  `Lumen.serve((request) => Response)` is the matching HTTP/1.1 **server** — not a WinterTC API,
  but the cross-runtime `serve(handler)` convention (Deno/Bun/Workers), so a Hono app runs with
  `Lumen.serve(app.fetch)`. v1 is single-accept, `Connection: close`, buffered bodies, http only
  (see `crates/lumen-web/src/server.rs`). Cold-start and usage: `examples/hono-app`.

- **Modules — both CommonJS and ESM.** `lumen-cli` picks the module kind the way Node does:
  `.mjs` is ESM, `.cjs` is CommonJS, `.js` follows the nearest `package.json` `"type"`.
  The CommonJS loader also keeps `.cjs` dependencies CommonJS inside `"type": "module"`
  packages, including callable `module.exports` with attached factory methods (as Jiti uses).
  ES modules run through the engine's real module graph (linking, top-level `await`); `import`
  specifiers resolve against disk and `node_modules`, `node:` builtins are importable
  (named imports included), and CommonJS packages interop by default export. CommonJS files
  run as the program entry with `require.main === module`.
  Package metadata reads root fields; exact ESM export subpaths and root exports stay separate.
  Exact relative package-private `#` imports support ordered `node`/`import`/`default`
  conditions: unmatched nested conditions continue to later siblings; null blocks resolution.
  Explicit exports suppress legacy entry fallback. Ordered arrays skip unmatched/null/invalid
  targets, without retrying later targets for missing files. ESM and CommonJS package subpaths
  support exact keys and single-star exports with exact-key and most-specific-pattern precedence.
  Pattern private imports and external private targets remain deferred.
  `require.resolve.paths()` reports the actual local ancestor lookup directories, returns null
  for builtins and the require's base directory for relative requests; global lookup directories
  are not advertised because this resolver does not search them.

- **`node:` compatibility** (`lumen-node`) — a CommonJS `require` with `node_modules`
  resolution and the module wrapper, `package.json` `main`/`exports`, the `node:path`/
  `node:os`/`node:fs` builtins, and `Buffer`, so packages written against the `node:` surface
  run. See the checklist at the top of `crates/lumen-node/src/lib.rs` for the deferred pieces
  (full export validation, the full N-API surface).

  `fs.globSync` walks real directories with a scoped cwd, pattern arrays, exclusions and
  Dirent results. It reuses the path matcher for wildcards, whole-segment globstars,
  character classes and same-segment brace alternatives; hidden paths require explicit
  dot segments. Extglobs, braces spanning directories and extra options throw unsupported
  errors; exclude callbacks currently require a globstar pattern. Symlink directories
  found during traversal are never descended, while explicit literal prefixes follow the
  ordinary filesystem lookup. Results use memory proportional to
  matches plus pending directory entries; no persistent scan cache is retained.

  `worker_threads.MessageChannel` uses native endpoint queues, Node/EventTarget listeners,
  synchronous `receiveMessageOnPort`, ref/unref/hasRef and close events. Worker messages
  transfer exclusive MessagePort ownership and share genuine SharedArrayBuffer backing via
  native capability envelopes; queued messages survive transfer. Fixed-size ordinary ArrayBuffers
  transfer through owned byte payloads and detach their sender backing only after validation;
  Buffer/subclass views decode to their intrinsic typed-array kind, preserving offsets and aliases.
  Numeric wire indexes only
  access capabilities admitted with that message, and each envelope caps attachments at 1024.
  Shared Atomics wait/notify checks and waiter registration prevent lost wakeups. Unsupported
  transfer types, resizable ordinary buffers and growable shared buffers throw. Ref state uses real timer handles;
  active ports poll at one millisecond, and close/transfer releases those handles. Queues,
  port handles awaiting explicit close and the engine's existing global shared-buffer registry
  have no automatic resource quota or finalizer: production resource limits remain necessary.

  String views created by pooled coroutine threads register with their owning driver,
  so return/drop/compaction uses one registry. Microtask drains defer moving view bytes while
  a started OS coroutine can retain parked native stack borrows; completion permits compaction.
  Lazy tagged-template identities also follow the driver, preventing a worker's cached template
  from aliasing an unrelated driver template. Module namespace metadata pins its object and
  retires with it during collection. Node `.js` worker entries honor package `type: module`,
  including suspended top-level await. `URL.parse()` follows the
  [URL Standard](https://url.spec.whatwg.org/#dom-url-parse), returning a live URL or null while
  preserving argument coercion errors.
  `worker_threads.SHARE_ENV` explicitly shares realm-owned environment backing with a child;
  ordinary workers keep copied state and nested sharing remains within its parent group.
  It never writes the embedder's OS environment. Each backing caps 16384 keys and 16 MiB;
  writes/deletions/enumeration and property descriptors see the same synchronized store.
  Local `structuredClone` transfers Node ports through native attachments, validates before
  getters and commits ownership only after successful serialization. Local shared-buffer
  clones retain genuine shared backing. Port polling drains queued messages before peer close.

  Synchronous `module.registerHooks` load hooks preserve CommonJS/ESM filename policy when
  their format is unspecified. An explicit `module` format and a transformed ESM source run
  through the module loader with relative imports and live namespace exports intact.

  `node:sqlite` supplies the core synchronous `DatabaseSync`/`StatementSync` API over the real
  native SQLite backend, including named/positional bindings, BigInt reads, iteration and native
  transaction state, location and column metadata. Double-quoted string literals and defensive
  mode use native connection flags. `LUMEN_SQLITE_LIBRARY` explicitly selects a library before the first open;
  its reported SQLite version is never overridden. Scalar `DatabaseSync.function` registrations
  retain their native API and JS callback until replacement/close, support BigInt arguments,
  varargs, deterministic/direct-only flags, and preserve thrown JS values. Registry admission
  caps at 256 name/arity entries per database; executing statements cannot be reentered or
  freed, while nested independent queries are supported. Callback engine pointers are bound
  at each step/exec, including after runtime moves. Authorizers, aggregate functions, sessions,
  backup, custom limits and enabling/loading extensions are not implemented; explicit unsupported
  constructor requests are rejected. `enableLoadExtension(false)` disables the real native
  connection API (or requires native proof that extension loading was compiled out), and
  databases created without extension permission reject attempts to enable/load extensions. Statements remain native-owned until
  the database closes; automatic statement finalization is not yet provided.

  **Native addons** load too: `require('./addon.node')` dlopens the compiled library and runs its
  N-API registration, resolving the addon's `napi_*` symbols against the lumen executable — the
  same mechanism the `node` binary uses. The N-API surface is implemented from scratch (values,
  properties, functions, callbacks, errors, references, object wrap, classes, promises, buffers,
  typed arrays, async work); the loader reaches `dlopen`/`dlsym` through raw `extern "C"`
  declarations. A realm owns one stable environment; calls refresh its engine pointer and
  delimit temporary handles. Registration and addon callbacks run on the realm driver thread,
  including calls made from nested pooled async/generator coroutines. Cleanup hooks run in
  reverse order before library unload. Threadsafe functions queue native payloads for owner-loop
  delivery with ref/unref and abort/backpressure: at most 128 per realm and 4096 queued payload
  pointers each. Native producers must join in cleanup; a producer surviving teardown prevents
  its context finalizer from running. Object-wrap GC finalizers and the full N-API surface remain
  incomplete; async-work execution is currently inline. Native addons have ambient machine-code
  authority and require external isolation for untrusted use. See `examples/native-addon`.

  Unix filesystem operations accept addon-opened descriptors through temporary CLOEXEC duplicates;
  only an explicit close takes ownership of closing the original. Standard streams retain realm
  routing. Same-runtime Unix subprocesses support a dedicated duplex IPC fd with JSON or
  structured-clone serialization, separate from stdout; frames cap at 8 MiB. This Lumen wire
  format is not Node's IPC wire format and is unavailable for foreign executables/Windows.

  `performance.markResourceTiming` records actual caller-supplied fetch measurements and
  notifies resource observers. Its resource buffer defaults to 250 and caps at 4096 entries;
  buffer-full event/overflow queue behavior remains unsupported. `process.getActiveResourcesInfo`
  remains a stub; require normal subprocess exit when checking cleanup.

  **`vite build` runs on lumen** (`examples/vite-app`): a full Vite production build, bundling
  through Rollup's native N-API addon, transforming with esbuild's service subprocess, over
  ESM↔CommonJS interop and the `node:` surface — building `dist/` and exiting cleanly.

- **REPL + CLI** (`lumen-repl`, `lumen-cli`) — an interactive shell with a persistent realm,
  parser-driven incomplete-input detection (multi-line continuation), top-level `await`, and
  loop-to-quiescence so timers and awaited promises settle before the next prompt. Line
  editing is line-buffered (raw-mode/history would need `termios`); use `rlwrap` for arrows
  and history.

- **Browser** (`lumen-wasm`, feature `runtime`) — `lumen-runtime` builds for
  `wasm32-unknown-unknown` and runs in a Worker: the same Buffer, crypto, zlib, timers, streams
  and `node:fs` (over an in-memory file system), with `fetch` and `WebSocket` bridged to the
  page's own. The event loop is single-threaded and driven by the page (`Runtime::run_until_idle`);
  a synchronous host call (`fs.readFileSync` of a remote or OPFS path) suspends the guest through
  a helper Worker and `Atomics.wait`. Sockets, subprocesses, native addons and sqlite throw
  `ERR_NOT_SUPPORTED_IN_BROWSER`. See `docs/browser-runtime.md` and `crates/lumen-wasm/example`.

### Workspace crates

```
lumen          engine (std-only, zero-dep; `embed` feature gates the runtime API)
lumen-common   engine-neutral code shared with lumen-py: bigint, Unicode, byte codecs, civil dates, tz tables; optional `hash` / `compress` features
lumen-os       engine-neutral OS services: file-system primitives, the `vfs` backends (OS, in-memory, overlay), errno
lumen-host     substrate: OpState, ResourceTable, Extension, the thread-pool/callback primitives
lumen-timers   setTimeout/setInterval/queueMicrotask/setImmediate
lumen-web      WinterTC Minimum Common API (Event, URL, crypto, fetch, …)
lumen-node     node: compatibility (require, node:path/os/fs, Buffer)
lumen-runtime  the event loop; assembles the op crates; console + process
lumen-repl     interactive shell
lumen-cli      node/deno-style entrypoint

test262-runner   conformance harness (parallel workers over ./test262)
lumen-difftest   differential fuzzer across the two execution tiers
lumen-wasm       wasm build of the engine; with `--features runtime`, `RuntimeSession`: the runtime in a browser
```

The dependency graph is a strict DAG — `lumen ← lumen-host ← {op crates} ← lumen-runtime ←
lumen-repl ← lumen-cli` — so each op crate can be worked on in isolation.

## Install

Grab a nightly prebuilt runtime on macOS arm64/x86_64 or Linux x86_64/arm64 (tagged releases
also publish Windows x86_64 binaries):

```sh
curl -fsSL https://raw.githubusercontent.com/lucid-softworks/lumen/main/scripts/install.sh | bash
```

It installs the `lumen` CLI to `~/.lumen/bin` from the rolling `nightly` release
(`LUMEN_INSTALL` and `LUMEN_RELEASE` override the location and tag). Other platforms build from
source — see below.

## Usage

Run scripts / open a REPL through the runtime:

```sh
cargo build --release -p lumen-cli
./target/release/lumen-cli                 # REPL (or: lumen-cli repl)
./target/release/lumen-cli file.js [args]  # run a script to loop quiescence
./target/release/lumen-cli -e 'code'       # evaluate a string
```

For memory-sanitizer diagnostics, building `lumen-cli` with `--features system-allocator`
uses Rust's system allocator instead of the size-class cache. This lets AddressSanitizer
observe individual frees and detect use-after-free inside Rust/host code; generated JIT
instructions are not sanitizer-instrumented. The feature is off by default and does not
change the normal runtime's allocator. With a nightly toolchain, an example build is:

```sh
RUSTFLAGS='-Zsanitizer=address -Cforce-frame-pointers=yes -Cdebuginfo=1' cargo +nightly build --profile fast --target aarch64-apple-darwin -p lumen-cli --features system-allocator
```

When building from Hashset, run this through its bounded Cargo wrapper and the shared target.
The size-class allocator's `mem-stats` allocation counters are unavailable in this mode.

With the normal allocator, `--features mem-stats` plus `LUMEN_MEM_STATS=1` prints a
memory breakdown at normal exit. Add `LUMEN_MEM_GC=1` to report at GC checkpoints
as well, useful when a constrained VM kills the process before exit. This diagnostic
walks the realm and is off by default. Major object sweeps or substantial cold-function
body retirement drain the allocator cache at most once per second per thread; macOS pressure relief
and glibc `malloc_trim` return unused pages where available. Musl and Android do not link
the glibc-only symbol.

Completed module initialization ASTs are released after success or terminal failure.
Live exports, their closures and environments, source positions, dependency ordering,
top-level-await metadata and cached errors survive. A module parked at an await keeps
its initialization body until settlement; imports never rerun a completed body.

Direct JIT calls use an interpreter-owned shadow stack. Every active frame carries its
owner's stack pointer; compiled code never embeds a coroutine pool thread's TLS address.
This prevents independent worker engines from sharing call records when cached code runs
on another thread. Storage is allocated lazily, bounded to 4 MiB per interpreter on 64-bit
Unix and 1 MiB on Windows/32-bit targets, and released with its interpreter. Untouched OS
pages consume no physical memory; a heap fallback keeps the same byte bound.

The engine also ships a minimal standalone shell (the test262 host, no runtime/host APIs):

```sh
cargo build --release -p lumen --bin lumen
./target/release/lumen file.js [more.js ...]
```

## Conformance

```sh
scripts/test262-clone.sh    # one-time: clone the suite into ./test262
scripts/run-test262.sh      # run it (see crates/test262-runner for env knobs)
LUMEN_TIER=bytecode scripts/run-test262.sh    # same suite against the bytecode tier
```

The execution tiers are also held together by a differential fuzzer: every generated program
runs in both tiers, which must agree on the completion value, thrown errors, the
observable side-effect trace, and final global state. Divergences are delta-minimized into a
regression corpus that replays on every run.

```sh
cargo run --release -p lumen-difftest -- --count 2000
```

## Benchmarks

For warm Map/Set build and lookup scaling at 100, 1,000, and 10,000 keys, run
`cargo bench -p lumen --bench collections`. Functions are defined once and reused
across samples; the small invocation expression still passes through `Engine::eval`.

```sh
scripts/run-v8bench.sh      # classic V8 suite (v8-v7) on lumen; downloads on first run
scripts/bench-compare.sh    # same suite on node + bun + lumen, as a markdown table

git clone https://github.com/chromium/octane.git ../octane   # one-time: Octane checkout
scripts/run-octane.sh                    # full Octane suite
scripts/run-octane.sh richards crypto    # selected benchmarks

git clone https://github.com/v8/web-tooling-benchmark ../web-tooling-benchmark   # one-time: checkout
(cd ../web-tooling-benchmark && npm install)                                     # one-time: build dist/cli.js
scripts/run-web-tooling.sh                    # full suite (babel, terser, acorn, etc.)
scripts/run-web-tooling.sh --only babel       # rebuild dist/cli.js for one selected benchmark
WEB_TOOLING_BENCHMARK_DIR=/path/to/web-tooling-benchmark scripts/run-web-tooling.sh --only terser
```

Octane is expected at `../octane` by default; set `OCTANE=/path/to/octane` to override.

Web Tooling Benchmark is expected at `../web-tooling-benchmark` by default;
set `WEB_TOOLING_BENCHMARK_DIR=/path/to/web-tooling-benchmark` to override. The
upstream CLI bundle does not support runtime benchmark selection; `--only <name>`
rebuilds `dist/cli.js` in that checkout with webpack's build-time selector
(`npx webpack --env.only=<name>`) before running lumen. The full suite is a
many-hours run on current lumen builds; prefer `--only <name>` while iterating.

### ARES-6

```sh
# one-time: provide an ARES-6 checkout outside this repo
# the default lookup is the sibling ../ARES-6; ARES6=... overrides it
scripts/run-ares6.sh                    # full ARES-6 suite
scripts/run-ares6.sh air basic          # selected workloads: air, basic, babylon, ml
ARES6=/path/to/ARES-6 scripts/run-ares6.sh babylon ml
```

ARES-6 sources are not vendored here. The runner expects a checkout at `../ARES-6` by default; set `ARES6=/path/to/ARES-6` to point at another checkout.

The reported `summary:` is the ARES-6 geomean in milliseconds, so lower is better. A selected run reports a partial geomean over only the selected workloads, which is useful for local iteration but is not the official full-suite ARES-6 score. If a workload fails, or if the expected metric and completion lines are missing, `scripts/run-ares6.sh` exits nonzero instead of hiding the failure.

Expect the full suite to take a long time on the current tree-walking engine; use selected workloads for quicker local checks.

### Portable SQLite embedding

`lumen-node`, `lumen-runtime` and `lumen-cli` expose the opt-in `bundled-sqlite` feature.
It links libsqlite3-sys 0.37's SQLite 3.51.3 and resolves the existing opaque-handle API
against static C entry points, including scalar callbacks, serialization and metadata.
Explicit custom-library selection still selects the whole dynamic API table; without the
feature the dependency-free system SQLite behavior is unchanged. Hashset enables the feature
for its embedded runtime. Original OpenClaw WAL-reset version checks are preserved.

The original Discord libopus-wasm codec now passes encode/decode in both main and real worker
realms. Its binary exposed incorrect numeric section ordering and stale completed-if labels;
WASM decoding now bounds section readers and checks DataCount, while branch/result stack
underflow traps rather than panicking. This verifies the codec, not a live voice gateway.


Disposed worker realms run an explicit cycle collection after their Runtime roots
are dropped, before publishing the exit event. The collector uses an uninitialized
interpreter without creating fresh builtin cycles and preserves external value
handles. Completed-worker allocator caches are released too. A panic skips this
collection. `collect_disposed_realms` is for a quiescent driver after realm disposal;
do not call it during active JS evaluation or while borrowing heap objects.
Focused core tests cover repeated closure/scope/prototype reclamation and external
handles; Web, Node and embedded-worker lifecycle checks cover normal/failed exits,
messaging, termination, top-level await and nested environment sharing.

Parsed functions compact their immutable parameter vectors before sharing,
releasing parser growth capacity without changing defaults, patterns, rest
arguments or reflection. `LUMEN_MEM_STATS=1` also enables numeric ESM-loader
retention checkpoints every 4,096 lookups, exposing counts/payload bytes rather
than paths or source. These payload counts exclude allocator overhead.

Main and worker event loops also collect on quiescent waits after substantial
setup allocation. The worker loop retains its cooperative 50-ms stop poll, and
the main loop includes the pending one-second collection deadline alongside
ordinary timers. This lets the follow-up pass release cold function bodies even
when no further message arrives; live closures and callbacks remain rooted.

The coroutine pool keeps at most eight completed helpers warm for immediate
reuse. After one second without a job, an unreserved offer is atomically removed
and its helper exits, dropping native cache, TLS and stack ownership. Checkout
and retirement share a mutex and exact allocation identity: a reserved helper
keeps waiting even when dispatch follows its timeout. Driver TLS is restored
before offering a helper. Active/suspended coroutines are not idle offers and
remain unaffected; this bounds idle retention rather than live concurrency.

Opt-in GC memory reports now include numeric module program state, retained
top-level statement slots and distinct captured source bytes. These describe
ModuleRec ownership, exclude nested AST allocations and never print module
paths or source. Existing completed/deferred/TLA/error and namespace checks
validate that adding the census leaves initialization and live bindings intact.

Immediately called block functions and dynamic/CommonJS wrappers parsed in lazy
mode retain their first validated body plus its original reload context. Cold
collection can release that AST and rebuild it on demand, just like skipped
bodies; explicit eager parsing remains eager. Captured scopes and source text
stay live, so this bounds cold AST retention rather than total source storage.

GNU/Linux opt-in memory checkpoints also report glibc arena allocation/free
bytes and directly mapped allocation bytes through optional `mallinfo2` lookup.
These measure allocator ownership, not RSS or source/AST totals. Older glibc
without that symbol simply omits this report; ordinary builds gain no allocation
headers or hot-path counters.

Suspended coroutine handoffs also drain unused native allocation caches after a
quiet second. They remain suspended with their activation and captured values
intact; cache trimming does not retire an active coroutine or run JavaScript.
Disconnected coroutines retain the existing permanent-park semantics but release
their unused allocation cache first.

Concise arrow expressions now retain reload metadata too. They keep their
validated first AST and original expression range, then reparse with lexical
context after cold collection. Their `expr_body` flag selects the expression
grammar; no synthetic source is allocated. Snapshot version 6 rejects older
format versions cleanly so callers can rebuild/reparse rather than interpret
new expression ranges as blocks.
