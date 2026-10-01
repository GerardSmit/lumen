# The runtime in the browser

- **Status:** In progress
- **TL;DR:** Build `lumen-runtime` (engine + Node/Web host APIs) for `wasm32-unknown-unknown` and
  run it in a browser Worker. Crypto and compression come from pure-Rust crates (RustCrypto,
  zlib-rs, brotli, a pure-Rust zstd) so the same code serves native and wasm. Host calls that are
  synchronous in Node but asynchronous in the browser (sync file reads over the network, a
  blocking `execSync` proxied to a server) suspend the guest through a Worker + `Atomics.wait`
  bridge, with JSPI as a leaner path where the browser supports it.

## Today

`lumen-wasm` wraps the bare engine for the playground. Nothing above the engine builds for wasm:
the event loop, timers and I/O completions use OS threads and `mpsc`; generators and async
bodies off the bytecode VM run on OS-thread coroutines (`crates/lumen/src/coroutine.rs`), which
fail to start on wasm32.

## Shape

```
page ──postMessage──▶ Worker: lumen-wasm (engine + lumen-runtime)
                        │  sync host call
                        ▼
                  SharedArrayBuffer slot ◀── helper Worker awaits the browser Promise
                  Atomics.wait / notify
```

1. **Event loop.** `lumen-runtime` gets a single-threaded loop mode: no thread pool, completions
   arrive from the embedder (browser Promises resolved in JS push completions into the loop),
   and the embedder drives turns (`Runtime::run_until_idle`, a "next timer due" query that the
   Worker turns into `setTimeout`). The threaded loop stays the native default.
2. **Module availability on wasm32.**
   - Pure computation, same code as native: path, util, events, assert, buffer, string_decoder,
     url, querystring, punycode, stream, readline (over a provided input), zlib, crypto,
     webcrypto, TextEncoder/Decoder, structuredClone, timers, console, diagnostics_channel,
     async_hooks, vm, Intl.
   - Bridged to browser APIs: `fetch`, `WebSocket`, `crypto.getRandomValues` (the `getrandom`
     crate's `js` feature), `performance`, `console`.
   - A virtual file system (in-memory, optionally backed by OPFS) behind `node:fs`, so sync and
     async fs calls keep their Node semantics.
   - Not available, throwing `ERR_NOT_SUPPORTED_IN_BROWSER`-style errors from the same
     validation path: `net`/`tls`/`dgram`/`http` servers, `child_process`, `cluster`,
     `worker_threads` (until Web Workers back it), N-API/`dlopen`, `bun:ffi`, sqlite (unless a
     wasm SQLite is added later).
   Gating is by `cfg(target_arch = "wasm32")` at the op-registration boundary; the JS builtins
   stay shared and see a missing binding.
3. **Coroutines.** Generators and async bodies must not need OS threads on wasm32: the bytecode
   VM already suspends async bodies itself; the remaining thread-based paths (tree-walker
   generators, generators the VM cannot compile) need a wasm path — compile-or-fail to the VM
   on wasm32, with the tree-walker tier unavailable there.
4. **Suspending a sync host call.** A native op marked *suspending* posts its request to a
   helper Worker and blocks on `Atomics.wait` over a `SharedArrayBuffer` until the helper has
   awaited the browser Promise and written the result. The guest observes an ordinary sync
   call; no other job of that realm runs meanwhile, so run-to-completion holds. Requires a
   cross-origin-isolated page (COOP/COEP). Where JSPI (`WebAssembly.Suspending`) is available
   the same op can suspend the wasm stack instead, without a helper Worker or isolation.
   Asyncify is rejected: it instruments the whole interpreter (30–50% size and speed cost).
5. **Crypto and compression.** RustCrypto on every platform for `node:crypto` and
   `crypto.subtle`; WebCrypto is not used underneath (async-only, small algorithm set, no
   incremental hashing). Compression through zlib-rs, `brotli`, and a pure-Rust zstd.

## Verification

- `cargo check --target wasm32-unknown-unknown -p lumen-runtime` in CI.
- A headless-browser smoke test (wasm-pack test or a small page) running a script that uses
  Buffer, crypto hashing/ciphers, zlib round trips, timers, fetch, and a sync fs read through
  the suspension bridge.
- The native Node compatibility suite (`scripts/run-node-compat.sh`) stays green.

## Open questions

- OPFS persistence vs. a pure in-memory fs by default.
- Whether `worker_threads` maps onto nested Web Workers.
