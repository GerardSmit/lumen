# Ahead-of-time compilation

Lumen currently produces AOT-BC: parsed AST headers, deferred function bodies,
bytecode and optional function source text. Functions that the bytecode compiler
refuses retain the tree-walker path. AOT-MC, with complete native functions and no
AST or bytecode, is not implemented. The current JIT still exits to the VM.

## Host tools

```text
lumen-cli target host.target
lumen-cli compile app.mts --tier bc --target host.target -o app.lbc
lumen-cli run app.lbc
```

`compile` uses the same walker as `include_js!`. Entries default to ES modules;
`--script` selects a classic script. `--node-modules` bundles resolvable package
dependencies, including CommonJS wrappers. `--keep-source` retains function text.
TypeScript uses the existing type stripper. An output that aliases any bundled
source is rejected. Native and Python compilation are rejected explicitly.
Target files currently contain the binary description of the running host engine;
device discovery and target presets are not implemented. AOT-BC target checks
require matching Lumen version and bytecode fingerprint, and a profile with a VM.

## Ownership and shared formats

`Precompiled::from_static` remains a constant constructor for embedded blobs.
`Precompiled::from_bytes(Arc<[u8]>)` wraps owned bytes; `Engine::load_precompiled_owned`
and `Runtime::run_precompiled_owned` are conveniences. Deferred AST, bytecode,
line tables and kept text retain shared byte ranges. Dropping the caller's blob
does not invalidate functions. `Precompiled` is `Clone`, and `as_bytes` borrows for
the lifetime of that handle; it is no longer `Copy` or universally static.
Static store caches keep their existing behavior. Owned stores avoid those
process-wide address-keyed caches, preventing stale entries after allocation reuse.

`lumen-common` owns the section-table codec, target wire encoding, byte-range
ownership, LZH and bounded decompression. The JS container preserves format 6 and
checks bytecode fingerprints. It rejects overlapping payloads, header/table aliases,
overflowing offsets, unsupported flags, duplicate manifests, duplicate line
sections and malformed manifest/line varints. Language-specific manifests and
AST/bytecode codecs remain in the engine. Explicit language tags and native
sections require a future container revision; Python remains independently owned.

`Engine::target_spec()` and `lumen::target::host()` report the linked engine.
`native_fp = 0` means no native loader. AArch64 features come from host detection
or the embedder's fixed system intersection; other targets currently report zero.
The target wire format
uses `LUMTGT01` and 72 little-endian bytes, with validated enum tags, architecture,
ABI, pointer width, profiles and execute-in-place ranges.

## Verification and measurements

Host tests cover ownership release, deferred bodies, bytecode, kept function text,
stack-trace lines, target/container rejection and a source-free CLI round trip.
`Precompiled::validate()` decodes all AST bodies and bytecode without running JS.
It reports corrupt chunks; lazy loading retains the AST compilation fallback.
Decoded chunks validate operand ranges, parameters, captures, function/name/cache
references, branches, stack merges, handler flow and the declared maximum stack.
Generator resumes and async iterator closing have their own stack effects. The
analysis is shared with JIT regions. The chunk fingerprint changed with the added
stack-bound field; the outer format-6 container remains unchanged.

`fuzz/fuzz_targets/precompiled.rs` parses container/target bytes, strictly validates
owned payloads and registers modules without executing JS. A deterministic test
mutates every byte of a generated blob with three replacement values. The Windows
runner `tools/verify/lumen-aot-fuzz.ps1` builds valid corpus/target seeds and runs
nightly cargo-fuzz with AddressSanitizer, bounded input size, time and memory.

The Windows host validation passed 853 `lumen` library tests, 144
`lumen-common` library tests, five `lumen-aot` tests across all targets and the
CLI round-trip test. The new fuzz target passed a nightly host `cargo check`.
These checks do not establish native execution or device installation support.

Initial release host measurements on the small Bitnest corpus (JIT eager mode):

| Program | Functions / chunks | Bytecode store bytes | LZH bytes | Exercised JIT bytes |
|---|---:|---:|---:|---:|
| arithmetic | 2 / 2 | 198 | 201 | 4,432 |
| objects | 6 / 6 | 635 | 530 | 34,172 |
| shell | 1 / 1 | 186 | 189 | 1,104 |

The LZH frame makes the two smaller stores larger. No compression default is
selected from this corpus. The larger test262/real-app corpus and native
function/code-cache measurements are still required.

The `lumen` example `aot_measure` accepts classic scripts and emits CSV for source,
function/chunk/refusal counts, raw and compressed blob sizes, bytecode-store LZH
size/host decode throughput, and exercised JIT region counts/bytes. Set
`LUMEN_JIT_EAGER=1` for reproducible compilation triggers. JIT region sizes are not
complete native AOT sizes; the measurement does not supply per-function native
results, parser/VM image deltas, device timings, RSS, code-cache variants or a
compression go/no-go decision.

## Remaining execution work

The fuzz campaign, native container revision and runtime feature split come next. The AOT-MC compiler
must replace every VM exit with an operation helper, landing pad or resumable
native state machine. It also needs closures/classes/modules, stack limits,
relocatable GOT-only output, a stable helper ABI and reproducible cross-target
code generation. The native loader, signing, XIP, size mode, profiles and device
installation depend on that execution model. A profile called `Aot` must never
silently retain a parser, interpreter or JIT.
