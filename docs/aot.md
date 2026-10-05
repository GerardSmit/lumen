# Ahead-of-time compilation

Lumen implements AOT-BC (AST headers, deferred bytecode and optional source) and
an AOT-MC producer/runtime path with boxed native lowering and AST-free function,
class, module and initialized-heap metadata. The current implementation pass also
adds compilerless Aot profiles, native glue, standalone packaging and link mode.
Host native compilation, execution, and the focused AOT tests have passed;
device and compression measurements remain open. AOT-BC retains its
AST fallback for refused bytecode functions; native producer failures are errors.

## Host tools

```text
lumen-cli target host.target
lumen-cli target-device COM3 device.target
lumen-cli inventory COM3 installed.inventory
lumen-cli compile app.mts --tier bc --target host.target -o app.lbc
lumen-cli run app.lbc
lumen-cli sign-native app.lmc --key seed.bin --output app.sig --public-key app.pub
lumen-cli pack-install app.lmc --name app --entry app.mts --output app.lumup --signature app.sig
lumen-cli upload COM3 app.lumup --target-out device.target
lumen-cli symbolize-native app.lmc app.lmc.map 3 120
lumen-cli symbolize-native app-with-lines.lmc 3 120
```

`compile` uses the same walker as `include_js!`. Entries default to ES modules;
`--script` selects a classic script. `--node-modules` bundles resolvable package
dependencies, including CommonJS wrappers. `--keep-source` retains function text.
TypeScript uses the existing type stripper. An output that aliases any bundled
source is rejected. Native and Python compilation are rejected explicitly.
The walker has a closed-world mode for native compilation: it requires literal
imports and requires to resolve into the bundle, rejects excluded dependencies,
and requires an explicit module list when computed dynamic imports are present.
It reports source locations of visible `eval` and Function-constructor calls
that will be unavailable in the native runtime, and records required built-in
modules in a sorted list for the native import table. The same checks cover
classic scripts and ES module entries.
The CLI exposes `compile --tier mc`, gated by native implementation readiness;
the new producer/runtime has not been built or tested during this pass.
`compile` also reads the first declared project configuration from
`package.json` (`lumen`), `pyproject.toml` (`tool.lumen`), `lumen.json`, or
`lumen.toml`; `--config` selects a file and command-line entry/tier/output
flags override project defaults. Bytecode-compatible entry, profile, module,
source-retention and package-walk settings are implemented, alongside native
metadata, trimming, signing, executable packaging and runtime variant selection.
Unsupported compression/resource-phase options fail explicitly.
Target files contain the binary description of the running engine. `--target @COM3`
queries a serial target directly; `@device` uses `LUMEN_AOT_DEVICE`. `target-device`
queries one serial device at 115200 baud (override with `--baud`) and writes its
target file; `inventory` prints policy-accepted installed apps, their source hashes,
and whether their fingerprint is stale, and saves the binary inventory. `upload`
queries the same target, rejects incompatible native blobs
before sending, then prints the device's install result. Named project targets
resolve descriptor/device and packaging defaults. AOT-BC target checks
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
AST/bytecode codecs remain in the engine. A separate format-7 native envelope now
tags JavaScript or Python and requires code, data and GOT sections while rejecting
AST/bytecode sections. The parser now validates the shared function table and
function GOT indices before a blob can be signed. The shared GOT codec validates ordered, complete slots and
resolves them before writing any pointer. A shared const-compatible native fingerprint
calculation covers target details, layouts, helper and static tables, and format versions;
the engine still reports `native_fp = 0` until complete ABI tables and execution
support exist. Language-specific function data and native JS execution remain
unimplemented; Python remains independently owned. Bitnest has an initial serial
install service, but the engine still advertises `native_fp = 0`, so it rejects
native apps.

`lumen-codegen::target_codegen::compile` selects the existing x86-64 or
AArch64 backend and ABI from `TargetSpec`, so an x86-64 host can generate
AArch64 instructions. `compile_aot` applies the size pipeline without loop
unrolling and converts direct calls to GOT references
using a front-end-supplied import-id to GOT-symbol mapping. The IR's
`symbol_addr` instruction also loads relocatable pointers through the GOT;
ordinary JIT linking rejects it. The JS front end still embeds other host
addresses and has not migrated to this instruction. AArch64 calls to the
same imported function share one GOT branch stub per compiled function;
`aot_image::link` fixes PC-relative displacements on the host, emits ordered
typed GOT relocations, folds functions with identical code and GOT references,
and page-aligns code before the writable GOT. Its
`encode_native` method writes the format-7 envelope. The
`encode_native_with_locations` method embeds validated function-relative
locations for local diagnostics. The
`encode_native_stripped` method emits a blob without device line tables and a
hash-bound host sidecar, validating each source location against that blob and
requiring normalized relative source paths. The
loader must preserve that relative virtual layout. XIP targets also align the
code section's file offset to a target page. The JIT front end still
embeds other runtime addresses, so these functions are not yet complete
AOT-MC output and no native-only blob is produced by the CLI.

The native data section has a shared version-3 prefix of validated function
code ranges and a sorted table of required built-in modules/native names with
signature hashes, followed by an opaque language-specific payload. Native GOT
import indices are checked against this table. The loader resolves every
required import before allocating executable memory and reports the missing name.
An optional `SEC_NATIVE_LINES` section carries versioned source names and
function-relative locations; the container rejects paths or offsets outside
the corresponding function. Stripped blobs omit the section and use the
hash-bound host sidecar instead.
The loader retains embedded locations in owned memory, so `LoadedNative::source_at`
can resolve a faulting PC without allocation.
`lumen-bind` descriptors now expose a hash of their generated argument, result,
role and parameter contract for the host/device native table.
`lumen-os::native::load` validates that prefix, the format-7 blob and target,
calls the embedder's signature-policy callback, allocates contiguous code/GOT
pages, resolves function slots and the remaining GOT entries, and seals only
copied code RX. For XIP placement an optional mapping callback maps the signed
code bytes RX beside fresh RW/NX GOT pages, without copying or sealing code.
The shared container also computes and validates the code/GOT mapping length,
including page separation and XIP section placement; the producer, uploader,
installer and loader call this same check. Bitnest rejects an install whose
mapping exceeds its available AOT reservation before replacing a storage slot.
Embedders must install a page mapping backend with cache maintenance.
`lumen-os::native::install_host_backend` now provides copied-code mappings on
Windows and non-macOS Unix hosts, using the existing OS page primitives and
sealing only code pages. The loader and host target share the OS-reported page
size, including `GetSystemInfo` on Windows; macOS
still needs a GOT-compatible `MAP_JIT` mapping backend.
Bitnest now registers a copied-code backend backed by an independent 8 MiB AOT
reservation and seals only the code prefix. Its JIT reservation is omitted
in the NoJit profile. XIP mapping remains unavailable there. The loaded image
exposes validated entry pointers by function index. It retains the hash of the
loaded blob and maps an in-range program counter to a `(blob hash, function
index, code offset)` location; folded functions use the lowest index at a
shared address. Kernel fault-hook registration and blackbox emission remain
outstanding.
JavaScript native metadata decoding and function entry dispatch are implemented.
JIT executable and scratch page allocation now live in `lumen-os::jitmem`, with
the old `lumen-codegen::jitmem` path re-exported for existing callers. This
removes an OS allocation dependency from the compiler backend's implementation;
the engine's JIT translation still depends on `lumen-codegen`. The new `jit`
feature defaults on. `compiler`, `jit` and `aot-native` now control source/VM,
JIT and native runtime code separately; compilerless Aot uses native internal
glue and explicit source-unavailable errors. Full and NoJit preserve source
support. The profile feature split and native glue passed host compilation and
focused tests; Bitnest profile GUI verification remains in progress. Bitnest
profile gates follow native readiness.
The target now reports its page size and built-in module-table hash. Host
linking uses the reported page size for the code/GOT boundary, and the mapping
backend rejects a mismatch. Empty/parallel and versioned host Node catalogs
have explicit table hashes; embedders freeze their ordered custom hash through
`lumen::target::set_builtin_modules_hash` before the first target query.

`lumen-crypto::native_signature` signs exact native
blob bytes with Ed25519 and verifies detached signatures against an allow-list.
`lumen-common::aot::install` defines length-bounded HELLO, TARGET, INSTALL and
RESULT frames, with a CRC covering each header and payload. INSTALL carries a
single-component app name, SHA-256 source hash and validated native blob.
The receiver can inspect the bounded frame length before allocating its buffer.
`install::Receiver` accepts partial serial/USB reads, validates the header before
reserving payload memory, and exposes a complete frame for CRC checking.
`authorize_install` then checks the running target and either a trusted
Ed25519 signature or an explicitly enabled unsigned development policy before
storage or mapping.
`lumen-cli sign-native` writes a detached Ed25519 signature from a 32-byte seed
and can export its 32-byte public key for a device allow-list;
`pack-install` creates the install frame. `--entry` walks the closed-world module
graph and hashes every source and package manifest read by the bundler. Use
`--node-modules` for package dependencies, `--module` for computed import targets,
and `--script` for scripts. Additional `--source` files are included; without a
walked entry/script/module, at least one `--source` is required. The hash covers
sorted paths relative to the current directory and their contents, with lengths
and a format tag in the SHA-256 input. `--source-root DIR` selects a different
root for graph paths and the hash. Sources outside that root are rejected.
Bitnest's registered `aot-install` service now recognizes the frame magic on
the existing console listener, answers HELLO with a target frame, and rejects
or stores INSTALL frames according to the target and signature policy. It writes
alternating generation-numbered, CRC-checked slots under `/apps`; slot selection
also rechecks the signature policy. Inventory reports the highest accepted generation
for each app as current or stale against the running target; it does not launch
an app. An otherwise compatible blob is stale if its code/GOT mapping exceeds
the device's current AOT reservation or violates page placement.
The sidecar codec (`lumen-common`'s `hash` feature) stores sorted function/code-offset line
entries and source names under the SHA-256 hash of the exact blob.
`symbolize-native` verifies that hash, validates function offsets against the
blob, then resolves a function index and code offset. Generating sidecars and
kernel crash-report integration remain to be wired.

`Engine::target_spec()` and `lumen::target::host()` report the linked engine.
`native_fp = 0` means no native loader. AArch64 features come from host detection
or the embedder's fixed system intersection; other targets currently report zero.
The target wire format uses `LUMTGT02` and 88 little-endian bytes, with
validated enum tags, architecture, ABI, pointer width, page size, built-in
module hash, profiles and execute-in-place ranges. Bytecode-only
`LUMTGT01` files remain readable; native targets must use version 2.

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
nightly cargo-fuzz with AddressSanitizer, bounded input size, time and memory. It
also generates a valid format-7 envelope with a GOT relocation for the native parser.
On 2026-10-02, the runner completed 2,012 inputs in 121 seconds on Windows/MSVC
without a crash. After adding the native seed, a second run completed 537 inputs
in 31 seconds without a crash. These short runs do not replace a sustained campaign.
With a GOT-bearing native seed, a five-minute follow-up completed 6,319 inputs
in 301 seconds without a crash.

The Windows host validation passed 853 `lumen` library tests, 144
`lumen-common` library tests, five `lumen-aot` tests across all targets and the
CLI round-trip test. The new fuzz target passed a nightly host `cargo check`.
After the GOT codec change, the targeted AOT suite passed 159 common, 10
precompiled, two `lumen-aot`, 62 bytecode and one CLI test. Bitnest's virt GUI
QEMU check and A7Z cross-build both passed.
These checks do not establish native execution or device installation support.

Initial release host measurements on the small Bitnest corpus plus Lumen's
self-contained HTTP parser fixture (JIT eager mode):

| Program | Functions / chunks | Bytecode store bytes | LZH bytes | Exercised JIT bytes |
|---|---:|---:|---:|---:|
| arithmetic | 2 / 2 | 198 | 201 | 4,432 |
| objects | 6 / 6 | 635 | 530 | 34,172 |
| shell | 1 / 1 | 186 | 189 | 1,104 |
| HTTP parser fixture | 55 / 55 | 15,708 | 9,481 | 978,367 (34 JIT units) |

The LZH frame makes the two smaller stores larger. No compression default is
selected from this corpus. The HTTP parser's exercised JIT code is about 62
times its bytecode store, despite compiling only 34 of 55 functions during
the run; this strongly motivates `-Osize` but is not an AOT-MC size forecast.
The larger test262/real-app corpus and native
function/code-cache measurements are still required.

`tools/verify/lumen-aot.ps1 -Measure` now writes `host-functions.csv` alongside
the summary. Each row pairs a precompiled function index and exact encoded
bytecode length with an exercised JIT compilation. In the HTTP parser fixture,
function index 39 has 4,123 bytecode bytes; two whole-function versions and one
loop version total 795,888 native bytes (81% of the fixture's observed JIT code).
The largest single version is 305,088 bytes. These are runtime variants, not
one AOT-MC function, and unexercised functions have no JIT row.
The verifier checks that every recorded JIT row has a function index and
bytecode size and that their native bytes sum to the summary total.

The `lumen` example `aot_measure` loads generated AOT-BC blobs and emits CSV for source,
function/chunk/refusal counts, raw and compressed blob sizes, bytecode-store LZH
size/host decode throughput, and exercised JIT region counts/bytes. Set
`LUMEN_JIT_EAGER=1` for reproducible compilation triggers. JIT region sizes are not
complete native AOT sizes; the measurement does not supply complete per-function native
results, parser/VM image deltas, device timings, RSS, code-cache variants or a
compression go/no-go decision.

As a full-profile image baseline, `llvm-size` reported 8,223,272 bytes of text,
121,840 bytes of data and 4,194,640 bytes of BSS for the verified virt kernel
on 2026-10-02. The A7Z cross-build reported 8,292,684 bytes of text, 121,736
bytes of data and 3,490,192 bytes of BSS. The parser/compiler/VM-free image
does not exist yet, so these baselines do not establish the saving from the AOT
runtime split.

## Remaining execution work

The runtime feature split and complete native data format come next. The AOT-MC compiler
must replace every VM exit with an operation helper, landing pad or resumable
native state machine. It also needs closures/classes/modules, stack limits,
relocatable GOT-only output, a stable helper ABI and reproducible cross-target
code generation. The native loader, signing, XIP, size mode, profiles and device
installation depend on that execution model. A profile called `Aot` must never
silently retain a parser, interpreter or JIT.
# Host configuration and deferred measurements

`compile` accepts `--profile full|nojit|aot`, repeated `--module FILE`,
`--source-root DIR` and `--compression false|auto|lzh`. AOT-BC compression
controls the source store; it does not compress native code. `sourceRoot`
(`source_root` in TOML) selects the module-key root. CLI values override project
defaults. Malformed higher-priority project files are reported during discovery.
`trim: false`, `stripLines: false`, and empty keep/assets/locales lists are valid
inactive settings; active unsupported native options still fail explicitly.

`--trim modules|members|aggressive` writes `app.trim.txt` with retained inputs,
required native modules, warnings and output size. The import walker retains
declared roots and their transitive imports; observable members are retained
conservatively.

The host `lumen-aot` feature `native` adds source-graph traversal, complete
function lowering, GOT linking, and versioned AST-free JS payload production.
The [payload schema](native-payload.md) documents the decoder contract. The CLI
native publishing path checks `native_aot::compiler_ready()` and the target's
native fingerprint. The host CLI test compiles and runs a bundled TypeScript
native image; Bitnest AOT profile GUI verification is still in progress.

Native images retain function-entry line locations by default. `--strip-lines`
or `stripLines: true` writes a hash-bound `APP.lmc.map` instead. Locations are
currently at function entries, not individual native instructions; synthetic
class expression functions have no entry location yet.

`lumen-cli runtime-imports APP.lmc ...` writes a deterministic JSON union of
required native modules/bindings for an app set. It rejects conflicting binding
signatures and incompatible fingerprints/versions within one language. This is
input for dedicated-runtime configuration; it does not identify every builtin
group reachable through dynamic global access or select Cargo features.

The Bitnest `tools/verify/lumen-aot.ps1` runner accepts an explicit `-Corpus`,
`-OutputDirectory`, and `-DeviceImages` list. `-Measure` records corpus/image
SHA-256 hashes and sizes, compares compressed and raw AOT-BC blobs, and requires
two identical compilations to produce identical bytes. `-Fuzz -FuzzSeconds N`
runs the precompiled payload fuzz target for a bounded campaign. These additions
have not been run during this implementation pass. They do not establish native
compression, device startup, steady-state or RSS results.

### Standalone host images and feedback

`compile ENTRY --exe -o APP` embeds the blob into the current CLI runtime stub;
`--stub FILE` selects an explicitly built runtime instead. Startup discovers the
payload before command dispatch, so arguments such as `compile` belong to the
embedded application. Windows uses `LUMENAOT` RCDATA and accepts
`--windows-subsystem console|gui`. ELF64 uses `.lumen_blob` and `.note.lumen`;
the runtime maps its executable read-only. Thin Mach-O64 uses `__LUMEN,__blob`
and requires a macOS host for ad-hoc signing. Mach-O stubs need header padding
and classic dyld fixups; chained-fixup stubs require relinking first. Signed PE
stubs must be supplied unsigned, then signed after embedding. Stub architecture
must match the target. The default stub is Full; NoJit/Aot profiles require an
explicitly built stub. Native standalone publishing has the same runtime
readiness gate as `.lmc` output.

`run --record-profile APP.prof ENTRY [ARGS...]` or
`record-profile -o APP.prof ENTRY [ARGS...]` records observed site types and
property shapes. `compile --tier mc --profile-data APP.prof` consumes the
versioned feedback; project configuration accepts `profileData`. A nonpreset
`--profile FILE` is also accepted as feedback input. `--profile full|nojit|aot`
retains its runtime-profile meaning. Feedback enables guarded specializations;
it never permits omitting cold branches. Standalone and feedback additions have
not been built or run during this implementation pass.

`--snapshot-at main` (configuration `snapshotAt`) requires a top-level function
declaration named `main`. Compilation runs initialization on the host and captures
its initialized global/module heap; the native metadata records the entry function
to invoke after restoring the graph. Initialization must not call `main`: direct
calls outside function bodies are rejected, and indirect calls during initialization
are unsupported. Pending host tasks, timers, termination, uninitialized modules,
unregistered native handles and ambiguous source-function identities are build
errors. Capture preserves cycles, closure environments and module live bindings;
the target restores native function references instead of replaying source.
CommonJS initialization uses a closed-world wrapper/require graph with lazy
evaluation, circular exports and explicit module/cache environment roots. ES
module facades initialize against a temporary require router, restored before
capture; mixed module initialization follows the module loader's dependency order.

Native compilation accepts `--signing-key env:NAME|file:PATH` and project
`signing.key`. Files contain the raw 32-byte Ed25519 seed; environment values
contain 64 hexadecimal digits. Compilation writes `OUTPUT.sig` for the native
blob. For standalone output this signs the embedded payload, not the OS executable;
platform executable signing remains a separate step. Output aliases of key files
and bundled sources are rejected.

`--mode link --runtime-lib FILE` emits a temporary deterministic COFF/ELF/Mach-O
object and invokes the native linker with the static runtime.
Build `lumen-exe-runtime` for the selected target/profile first. Its C `main`
loads the linked `lumen_aot_blob_start/end` slice and passes process arguments to
the application. `--linker PROGRAM` selects a toolchain, and repeatable
`--link-arg ARG` supplies the libraries/flags reported by Rust's
`--print native-static-libs` for that build. Runtime Cargo features select Full,
NoJit or compilerless Aot and optional Node groups. MC objects place native text,
GOT and unwind records in static sections; BC objects contain only the blob and
empty linked-code ranges. The runtime validates linked code against the authentic
container before invoking it. This has not been built or linked during this pass.

Native imports are checked against the target's exact builtin catalog. The
versioned parallel catalog permits only `lumen:parallel`; an empty catalog
permits none. Known host Node catalogs name exact bare/`node:` namespaces and
optional HTTP2/cluster/dgram/WASI groups. Custom tables require `--builtin-catalog FILE` (configuration
`builtinCatalog`): JSON `{"format":1,"imports":[{"module":"example",
"name":"","signatureHash":"0123456789abcdef"}]}`. Entries are unique,
sorted by module/name, and use nonzero version signatures. Their canonical
`builtin_modules_hash` must equal the target hash. Module requirements emitted
in the container use the format's module-only signature `0`; catalog version
signatures remain part of the table identity. Source-backed Node/Bitnest modules
are not inferred from a nonzero hash.

Embedders fix `target::set_builtin_modules_hash` before querying a target or
creating a realm. They register each native namespace with
`Engine::register_native_module` before loading a blob. Named native bindings
also require `unsafe Engine::register_native_binding(module,name,signature_hash,
address)` against the custom catalog; its address and signature must implement
the declared calling contract. The host manifest, target hash and per-realm
registrations must agree exactly.

`--trim members|aggressive` applies conservative module-function reachability:
top-level uses, exported functions and `--keep PATTERN` are roots; transitive
function references retain callees. Unreachable declarations and their nested
functions/native metadata are removed. Object/class members, scripts/CommonJS
namespaces and snapshot lexical environments are retained with report warnings.
Both levels currently use this conservative analysis; aggressive does not remove
reflection-visible members. `OUTPUT.trim.txt` records kept units, removals,
warnings, native dependencies and blob bytes.

Project `targets` entries select named output defaults with `--target NAME`.
They accept `exe`/`out`, `os` (`linux`, `windows`, `macos`), `arch`
(`x86_64`, `aarch64`), `mode`, `subsystem`, `icon`, `stub`, `runtimeLib`,
and either `descriptor` or `device`. Paths resolve against the project file;
CLI flags override named defaults. Cross architecture and runtime profile
selection require a matching runtime descriptor or queried device. Link mode
selects ELF/COFF/Mach-O from `os` or `--os`, including macOS x86-64 where the
calling convention alone cannot identify the object format. Windows ARM64
standalone output uses the Windows ARM64 calling convention and unwind records.

`runtimeCatalog` or `--runtime-catalog FILE` selects prebuilt runtime variants;
`LUMEN_RUNTIME_CATALOG` and `runtime-catalog.json` beside the CLI are discovery
options. Format 1 contains a `variants` array. Each entry declares `os`, `arch`,
`profile`, `features`, `descriptor`, and `stub` and/or `runtimeLib`; optional
`locales` records its compiled locale selection. Paths resolve against the catalog.
Selection computes the app's import/source feature union and chooses the smallest
compatible feature/locale set. The descriptor must match the compilation target.
`OUTPUT.runtime.json` records the runtime Cargo arguments and locale environment.
Core builtin groups remain in the runtime; selection currently trims existing
optional Cargo groups. The catalog must describe the actual built runtime.

`assets` and repeatable `--asset GLOB` embed deterministic, sorted file archives.
Configuration patterns resolve against the project directory, independent of
module `sourceRoot`. Symlinks are skipped and unmatched patterns are errors.
Standalone startup mounts assets read-only at `/lumen-assets`; for example
`node:fs.readFileSync('/lumen-assets/assets/logo.svg')`. Filesystem open/read/stat
use the existing VFS overlay, and writes to embedded paths fail. Embedders can use
`Runtime::install_embedded_assets` or the borrowed common
`aot::assets::from_blob(...).get(path)` archive API.

`locales` or `--locales en,nl` selects runtime locale data through the generated
build plan's `LUMEN_LOCALES` environment. English fallback is retained. An omitted
selection retains all locales. Selected runtime builds omit locale formatting
branches and stop advertising omitted languages; shared canonicalization and
regional tables remain. Prebuilt variants must be rebuilt with this environment
or selected from a matching runtime catalog to realize data reduction.

CLI trimming defaults to `members` for MC and `modules` for BC. Explicit
`--trim=false` disables it. Aggressive dynamic-access/reflection warnings become
errors unless a whole-unit keep root resolves them. The trim report includes
removed declaration/source sizes and kept code/metadata sizes with reasons.

Link mode consumes a static runtime archive, creates an exclusive temporary
object, links with section collection and deterministic identifier options, and
removes that temporary object after success or failure. It discovers bundled
`rust-lld` beside the CLI or in the current Rust sysroot. Unix uses Clang for
CRT/SDK search paths when available, then the system `cc`; Windows falls back
to `link`. `--linker`, `LUMEN_LINKER`, or Unix `CC` overrides discovery. Target
SDK/CRT and native dependency libraries are still required; cross-OS links need
an explicit linker and SDK arguments through `--link-arg`.

On Unix `--linker` selects a compiler driver, such as `clang`; raw LLD cannot
supply CRT/SDK paths through this interface. Select it with
`--link-arg=-fuse-ld=PATH`. Windows `rust-lld` receives `-flavor link`.

Build a host link-runtime catalog with
`tools/build-lumen-runtimes.ps1 -OutputDirectory PATH -Profiles aot,nojit,full`.
`-Features parallel,intl` adds optional groups; `-Locales en,nl` selects primary
language data. The script builds each staticlib with isolated Cargo output,
runs a matching-feature descriptor probe, and publishes `runtime-catalog.json`
only after all variants succeed. It produces host link variants; cross-target
descriptor probes require execution on the corresponding target host. Stub
variants must be supplied separately. This workflow has not yet been executed.

`tools/verify/lumen-aot.ps1 -Measure` now produces `native-variants.csv`
(untrimmed host native blob sizes and repeated-build SHA256 comparison),
`native-compression.csv` (whole-container and individual-section LZH frame
sizes, median/min/max decode latency and MiB/s), and
`native-compression-environment.json` (host/compiler identity and method).
The codec uses five batches lasting at least 100ms each, excluding file IO and
compression while including decoder allocation/free. Round trips and encoder
determinism are checked before timing. These additions have not yet run.
The generated frames are experiments, not runnable compressed native images.
Host decode rates cannot establish target-core startup, RSS, cache pressure or
steady-state performance; compression defaults remain unchanged pending those
measurements.
