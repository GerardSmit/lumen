//! lumen — a from-scratch JavaScript engine (std-only, no dependencies).
//!
//! lumen is the eventual in-house replacement for the V8 backend in the `js` crate. Today it is a
//! tree-walking interpreter covering the ECMAScript language core, driven by the tc39/test262
//! conformance suite (see `crates/test262-runner`). It deliberately implements a growing *subset* —
//! the test262 score is the roadmap.
//!
//! ## Shape
//! - [`lexer`] tokenizes, [`parser`] builds the [`ast`], [`interpreter`] + `eval` walk it.
//! - [`value`] is the prototype-based object model (`Gc` (`ObjCell`), reference-counted — no
//!   real GC yet, so reference cycles leak; fine for the per-test runner).
//! - [`builtins`] installs the realm (`globalThis`, `Object`/`Array`/`Function`/`Math`, the error
//!   constructors, global functions).
//!
//! ## Public API
//! [`Engine::new`] builds a fresh realm; [`Engine::eval`] runs a script and reports a [`Completion`]
//! (a value, or a thrown error with its constructor name + message) or a parse-phase [`ParseError`].
//! The error name + phase distinction is exactly what a test262 negative-test matcher needs.

// The ECMAScript abstract operations (`to_number`/`to_string`/`to_primitive`/…) take `&mut self`
// on purpose: converting an object can run user `valueOf`/`toString`/getters, which mutate the
// realm. That trips clippy's `wrong_self_convention`, which assumes `to_*` is a cheap borrow.
#![allow(clippy::wrong_self_convention)]

mod ast;
use lumen_common::bigint;
mod builtins;
pub use lumen_common::buffer;
pub mod bytecode;
mod console_fmt;
mod coroutine;
/// Typed Rust <-> JS conversions and the runtime of the binding macros (see [`embed`]).
#[cfg(feature = "embed")]
mod embed_convert;
mod eval;
mod native_ops;
#[cfg(feature = "compiler")]
pub mod feedback;
#[cfg(feature = "compiler")]
pub mod heap_snapshot;
#[cfg(all(feature = "compiler", feature = "jit"))]
mod snapshot_cjs;
/// The engine's size-class caching allocator — allocation-bound workloads (one refcounted box
/// per JS object/scope) run 15-30% faster than on the system allocator. NOT registered here: a
/// library must not preempt an embedder's `#[global_allocator]` (the test262 runner caps
/// worker allocations with its own). Binaries opt in:
/// `#[global_allocator] static A: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;`
#[cfg(not(target_arch = "wasm32"))]
pub use lumen_common::fastalloc;
pub use lumen_common::limits;
pub mod gc_log;
use lumen_common::fasthash;
mod host;
mod interpreter;
#[cfg(feature = "intl")]
mod intl;
mod jit_ir;
mod jstr;
#[cfg(feature = "compiler")]
mod lexer;
mod lstr;
mod modules;
#[cfg(feature = "parallel")]
pub mod parallel;
#[doc(hidden)]
pub use modules::load_stats;
#[cfg(feature = "intl")]
mod numbering;
use lumen_common::lzh;
/// Opt-in memory accounting (`LUMEN_MEM_STATS=1`).
pub mod memstats;
mod parser_support;
#[cfg(feature = "compiler")]
mod parser;
#[cfg(not(feature = "compiler"))]
#[path = "parser_unavailable.rs"]
mod parser;
#[cfg(feature = "aot-native")]
pub mod native_aot;
pub mod precompiled;
pub mod target;
mod regex;
mod snapshot;
mod split_view;
mod str_index;
use lumen_common::stack;
mod sync_callbacks;
mod temporal;
mod token;
use lumen_common::tz;
mod local_tz;
pub mod typescript;
#[rustfmt::skip]
mod umalqura;
#[cfg(feature = "intl")]
mod cldr_pack;
#[rustfmt::skip]
#[cfg(feature = "intl")]
mod cldr_likely;
#[rustfmt::skip]
#[cfg(feature = "intl")]
mod cldr_dates;
#[rustfmt::skip]
#[cfg(feature = "intl")]
mod cldr_units;
#[rustfmt::skip]
#[cfg(feature = "intl")]
mod tznames;
#[rustfmt::skip]
mod units;
use lumen_common::unicode_norm_impl;
use lumen_common::unicode_props;
mod value;

use interpreter::Interp;
use value::Value;

/// Internal-stage entry points, exposed only for benchmarking (`bench` feature). These reach past
/// the stable public API to time individual compilation stages (lex → parse → snapshot encode →
/// decode) — the breakdown behind cold-boot cost. Not a stability commitment; do not depend on it.
#[cfg(feature = "bench")]
pub mod bench_api {
    pub use crate::ast::Stmt;
    pub use crate::lexer::tokenize;
    pub use crate::parser::{parse_module, parse_module_jsx, parse_module_ts, parse_script};
    pub use crate::snapshot::{decode, encode};
}

/// Host wall-clock override: milliseconds since the Unix epoch. Targets without a usable
/// `SystemTime` (wasm32-unknown-unknown) install one at startup; when unset, `Date`/`Temporal.Now`
/// fall back to `SystemTime`.
static HOST_CLOCK: std::sync::OnceLock<fn() -> f64> = std::sync::OnceLock::new();

/// Install a process-wide wall-clock source (first call wins). The embedder's `f` returns
/// milliseconds since the Unix epoch.
pub fn set_host_clock(f: fn() -> f64) {
    let _ = HOST_CLOCK.set(f);
}

pub(crate) static TAIL_CALLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turn proper tail calls (ES2015 PrepareForTailCall) off for this process. V8 has none, and
/// hosts that mirror its stack traces need every caller's frame to stay visible.
pub fn set_tail_calls(on: bool) {
    TAIL_CALLS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Set the process-wide local time zone (V8 keeps one per process too): the zone of `Date`'s
/// local-time methods and `toString`, the default `Intl.DateTimeFormat` time zone and
/// `Temporal.Now.timeZoneId`. `name` is an IANA zone (`"Europe/Paris"`; POSIX `TZ` spellings such
/// as `":Europe/Paris"` or a `.../zoneinfo/Europe/Paris` path work too); `None` means UTC, the
/// default. Takes effect for every later call. Returns false, and uses UTC, if `name` is unknown.
pub fn set_local_time_zone(name: Option<&str>) -> bool {
    local_tz::set(name)
}

/// The current local time zone's identifier ("UTC" unless [`set_local_time_zone`] chose another).
pub fn local_time_zone() -> &'static str {
    local_tz::id()
}

pub(crate) fn tail_calls_enabled() -> bool {
    TAIL_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Install the WebAssembly JIT host (first call wins; meaningful on wasm32 only). On wasm32 the
/// optimizing tier compiles hot loops to small WebAssembly modules that import the engine's own
/// `env.memory` and `env.table` (the indirect-function table, which must be growable — link with
/// `--growable-table`). `f` receives a module's bytes, instantiates it with those imports, appends
/// its export `f0` to the table and returns that table index — or `None` (the loop then stays
/// interpreted). Without a host the tier is off on wasm32.
pub fn set_wasm_jit_host(f: fn(&[u8]) -> Option<u32>) {
    let _ = WASM_JIT_HOST.set(f);
}

pub(crate) static WASM_JIT_HOST: std::sync::OnceLock<fn(&[u8]) -> Option<u32>> =
    std::sync::OnceLock::new();

/// The installed host clock's current time, if one was set.
pub(crate) fn host_now_ms() -> Option<f64> {
    HOST_CLOCK.get().map(|f| f())
}

pub use lumen_os::jitmem::{NativeBackend, install_native_backend as install_native_jit_backend};

/// Native code lifetime notification. The name is borrowed only during the callback.
pub struct JitCodeEvent<'a> {
    pub name: &'a str,
    pub start: usize,
    pub length: usize,
    pub loaded: bool,
}
static JIT_CODE_HOOK: std::sync::OnceLock<fn(JitCodeEvent<'_>)> = std::sync::OnceLock::new();

/// Install once before creating realms. Called on each realm's owner thread, outside IRQs.
pub fn set_jit_code_hook(hook: fn(JitCodeEvent<'_>)) -> Result<(), fn(JitCodeEvent<'_>)> {
    JIT_CODE_HOOK.set(hook)
}

pub(crate) fn jit_code_event(event: JitCodeEvent<'_>) {
    if let Some(hook) = JIT_CODE_HOOK.get() { hook(event); }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitStats {
    pub compiled_units: u64,
    pub code_bytes: usize,
    /// Native entries that returned to the interpreter; generated direct calls
    /// inside one such entry are not counted separately.
    pub executed_entries: u64,
    pub compile_nanoseconds: u64,
    pub failed_compilations: u64,
    pub allocation_failures: u64,
    /// Compiled units released to make room for new code (memory pressure).
    pub evictions: u64,
    pub evicted_bytes: usize,
}

#[cfg(feature = "bench")]
#[derive(Clone, Copy, Debug)]
pub struct JitCompilation {
    pub function_index: Option<usize>,
    pub bytecode_bytes: Option<usize>,
    pub kind: &'static str,
    pub op_count: usize,
    pub native_bytes: usize,
}

impl core::ops::AddAssign for JitStats {
    fn add_assign(&mut self, other: Self) {
        self.compiled_units += other.compiled_units;
        self.code_bytes += other.code_bytes;
        self.executed_entries += other.executed_entries;
        self.compile_nanoseconds += other.compile_nanoseconds;
        self.failed_compilations += other.failed_compilations;
        self.allocation_failures += other.allocation_failures;
        self.evictions += other.evictions;
        self.evicted_bytes += other.evicted_bytes;
    }
}

/// Owner-local JIT policy, independent of process environment variables.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JitMode {
    Disabled,
    #[default]
    Hot,
    Eager,
}

/// Parse `src` as a script and encode its AST to a snapshot blob — a build-time helper (used
/// from op crates' `build.rs`) so static JS glue is parsed once at build and decoded, not
/// re-parsed, on every boot. Decode it at runtime with [`Engine::eval_snapshot`], handing it the
/// same `src`: the snapshot's functions carry byte ranges into it, not copies of it, and decode
/// with their bodies unparsed until first call. `Err` is a parse-error message — from anywhere
/// in `src`, including bodies the snapshot leaves unparsed.
pub fn compile_snapshot(src: &str) -> Result<Vec<u8>, String> {
    let fmt = |e: parser::ParseError| format!("{} (line {})", e.message, e.line);
    parser::with_eager_bodies(|| parser::parse_script(src, false)).map_err(fmt)?;
    let body = parser::parse_script_lazy(src).map_err(fmt)?;
    Ok(snapshot::encode(&body, src))
}

pub use parser::with_eager_bodies;
pub use parser::{JsxOptions, JsxRuntime};
#[cfg(feature = "compiler")]
pub use parser::transpile_jsx;
pub use stack::{set_thread_stack_bounds, set_thread_stack_size, THREAD_STACK_SIZE};

/// Parse `src` without running it: as an ES module when `module`, otherwise as a CommonJS
/// module body (where a top-level `return` is legal). Node's `--check`.
pub fn check_syntax(src: &str, module: bool) -> Result<(), ParseError> {
    let fmt = |e: parser::ParseError| ParseError {
        message: e.message,
        line: e.line,
        at_eof: e.at_eof,
    };
    if module {
        parser::parse_module(src).map(|_| ()).map_err(fmt)
    } else {
        parser::parse_cjs_function(src, &[], false)
            .map(|_| ())
            .map_err(fmt)
    }
}

/// A JS string's text as well-formed UTF-8 (lone surrogates become U+FFFD): what encoders
/// such as `TextEncoder` write, as opposed to the engine's internal form.
pub fn well_formed_utf8(s: &str) -> std::borrow::Cow<'_, str> {
    jstr::well_formed(s)
}

/// Ahead-of-time compilation (see [`Engine::load_precompiled`] and the `lumen-aot` crate):
/// [`precompile`] / [`precompiled::PrecompileBundle`] run at build time and produce a
/// source-free blob; [`Precompiled`] wraps one linked into the binary.
pub use interpreter::{AtomicsWaitEvent, AtomicsWaitPhase};
pub use precompiled::{precompile, Precompiled, SourceKind};

/// A parse-phase failure. test262 reports these as a `SyntaxError` thrown during parsing.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub line: u32,
    /// The parse failed only because the input ended too soon (e.g. an unclosed block or
    /// template). A REPL treats this as "keep reading lines", not a SyntaxError.
    pub at_eof: bool,
}

/// The outcome of evaluating a script.
pub enum Completion {
    /// Ran to completion; the last statement value rendered to a string (best-effort).
    Value(String),
    /// A value was thrown. `name` is the error's constructor name (`"TypeError"`, …) when the
    /// thrown value is an Error object, else `""`.
    Throw { name: String, message: String },
}

/// Parse `src` as a plain script without running it.
pub fn check_script_syntax(src: &str) -> Result<(), ParseError> {
    parser::parse_script(src, false)
        .map(|_| ())
        .map_err(|e| ParseError {
            message: e.message,
            line: e.line,
            at_eof: e.at_eof,
        })
}

/// A JavaScript engine instance: one realm (global object + intrinsics) that persists across
/// [`eval`](Engine::eval) calls.
pub struct Engine {
    interp: Interp,
}

/// Collect unreachable object/scope cycles after all realms on this driver have been
/// disposed. Call only at a quiescent point, never during an active JS evaluation or
/// while borrowing its objects. External value handles stay roots. This lets a worker
/// reclaim dead cycles before its thread-local heap teardown would leak live chunks.
pub fn collect_disposed_realms() {
    collect_quiescent_realms();
}

/// Evict native code while retaining bytecode and realm state.
///
/// # Safety
/// All engines on the calling thread must be idle, with no executing or suspended
/// native frames or retained raw entry pointers. See the code-cache hook.
pub unsafe fn evict_quiescent_jit_code() -> usize {
    let evicted = unsafe { bytecode::jit::evict_quiescent_code() };
    collect_quiescent_realms();
    evicted
}

/// Collect retired realm cycles while other engines remain alive on this driver.
/// Every engine must be idle: no suspended JS evaluation, callback, or borrowed
/// object. Live engines and external value handles stay roots. Embedders that
/// cannot ensure this boundary must wait until all realms have been disposed.
pub fn collect_quiescent_realms() {
    let before = memstats::enabled().then(crate::value::live_objects);
    let mut collector = Interp::uninitialized();
    let mut previous = crate::value::live_objects();
    loop {
        collector.gc_collect();
        let remaining = crate::value::live_objects();
        // Sweeping a retired function can drop native code whose constant
        // handles rooted other cycles during this pass. Finish those next.
        if remaining >= previous { break; }
        previous = remaining;
    }
    drop(collector);
    crate::value::gc_trim_quiescent_heap();
    if let Some(before) = before {
        eprintln!(
            "[disposed-realm] before={before} after={}",
            crate::value::live_objects()
        );
    }
    // A completed worker will not reuse these allocator caches. Release them now,
    // including free glibc pages on GNU Linux, rather than retaining startup RSS.
    #[cfg(not(target_arch = "wasm32"))]
    crate::fastalloc::trim();
}

#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod realm_cleanup_tests;
#[cfg(test)]
mod stack_tests;

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static LIVE_ENGINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Dropping the last engine on a thread collects the cycles its realm leaves behind (closures,
/// prototypes, the intrinsics themselves), as [`collect_disposed_realms`] does. While another
/// engine on the thread is alive it may be mid-evaluation, so collection waits for the last one.
impl Drop for Engine {
    fn drop(&mut self) {
        let last = LIVE_ENGINES
            .try_with(|n| {
                n.set(n.get().saturating_sub(1));
                n.get() == 0
            })
            .unwrap_or(false);
        if !last || std::thread::panicking() || !value::gc_state_alive() {
            return;
        }
        drop(std::mem::replace(&mut self.interp, Interp::uninitialized()));
        self.interp.gc_collect();
        value::gc_trim_quiescent_heap();
        // Native owning cores stay online after their final worker exits. Return
        // cached free blocks now rather than waiting for nonexistent thread exit.
        #[cfg(target_os = "none")]
        crate::fastalloc::trim();
    }
}

impl Engine {
    /// Select before evaluating scripts; existing chunks may retain native code.
    pub fn set_jit_mode(&mut self, mode: JitMode) {
        self.interp.jit_mode = mode;
    }
    /// Select compact (16) or measured hot (64) native code alignment for future compilations.
    /// Existing code remains valid; extra padding stays subject to the host arena budget.
    pub fn set_jit_alignment(&mut self, alignment: usize) -> bool {
        if !matches!(alignment, 16 | 64) { return false; }
        self.interp.jit_alignment = alignment;
        true
    }
    pub fn jit_stats(&self) -> JitStats {
        self.interp.jit_stats.get()
    }
    /// Experimental native loop prefetches for future compilations; disabled by default.
    pub fn set_jit_prefetch(&mut self, enabled: bool) {
        self.interp.jit_prefetch = enabled;
    }

    #[cfg(feature = "bench")]
    pub fn jit_compilations(&self) -> Vec<JitCompilation> {
        self.interp.jit_compilations.borrow().clone()
    }

    /// Collect unreachable object/scope cycles between evaluations. External
    /// value handles and this realm's global bindings remain roots.
    pub fn collect_garbage(&mut self) -> i64 {
        self.interp.collect_garbage_for_host()
    }

    pub fn new() -> Engine {
        let _ = target::cpu_features();
        interpreter::sym_for_reset();
        let _mem = memstats::enter(memstats::Cat::Builtins);
        let _ = LIVE_ENGINES.try_with(|n| n.set(n.get() + 1));
        Engine {
            interp: Interp::new(),
        }
    }

    /// Run `src` as a spawned `$262.agent`: the agent may block in `Atomics.wait`, receives
    /// SharedArrayBuffer broadcasts on `broadcast_rx`, and reports back via `report_tx`.
    /// Whether this agent may block in `Atomics.wait` (test262's CanBlockIsTrue flag).
    pub fn set_can_block(&mut self, b: bool) {
        self.interp.can_block = b;
    }

    /// Observe every synchronous `Atomics.wait` of this realm.
    pub fn set_atomics_wait_hook(&mut self, hook: Option<Box<dyn Fn(&AtomicsWaitEvent)>>) {
        self.interp.atomics_wait_hook = hook;
    }

    pub fn run_as_agent(
        &mut self,
        src: &str,
        broadcast_rx: std::sync::mpsc::Receiver<(u64, usize)>,
        report_tx: std::sync::mpsc::Sender<String>,
    ) {
        self.interp.can_block = true;
        self.interp.agent = Some(Box::new(interpreter::AgentChannels {
            agent_broadcast_txs: Vec::new(),
            report_rx: None,
            report_tx,
            broadcast_rx: Some(broadcast_rx),
        }));
        let _ = self.eval(src, false);
    }

    /// Parse and run `src`. `strict` forces strict mode (used for the test262 strict variant); a
    /// `"use strict"` directive in the source also enables it.
    pub fn eval(&mut self, src: &str, strict: bool) -> Result<Completion, ParseError> {
        if native_ops::dynamic_code_disabled() {
            return Err(ParseError { message: "dynamic code is unavailable in native execution".into(), line: 0, at_eof: false });
        }
        let body = parser::parse_script(src, strict).map_err(|e| ParseError {
            message: e.message,
            line: e.line,
            at_eof: e.at_eof,
        })?;
        // A top-level `"use strict"` directive prologue turns on strict mode for the whole script.
        let directive_strict = matches!(
            body.first(),
            Some(ast::Stmt::Expr(ast::Expr::Str(s))) if &**s == "use strict"
        );
        self.interp.strict = strict || directive_strict;
        let result = self.interp.run_program_parsed(&body);
        // Run queued promise reactions (the microtask checkpoint after the script).
        self.interp.run_agent_event_loop();
        match result {
            Ok(v) => Ok(Completion::Value(self.render(&v))),
            Err(thrown) => Ok(self.describe_throw(thrown)),
        }
    }

    /// Like [`eval`](Engine::eval), but the script body comes from a precompiled snapshot blob
    /// (see [`compile_snapshot`]) instead of parsing `src` — the runtime uses this to skip
    /// re-lexing/parsing its static JS glue on every boot. `src` must be the text the snapshot
    /// was compiled from: decoded functions slice their `toString` text and lazily parsed bodies
    /// out of one shared copy of it. A decode failure (version skew, corruption, another source)
    /// surfaces as `Err(ParseError)` so the caller can fall back to `eval` on `src`; the
    /// resulting AST is otherwise identical to a parsed one, so execution is byte-for-byte the
    /// same.
    pub fn eval_snapshot(
        &mut self,
        bytes: &[u8],
        src: &str,
        strict: bool,
    ) -> Result<Completion, ParseError> {
        if native_ops::dynamic_code_disabled() {
            return Err(ParseError { message: "source snapshots are unavailable in the Aot profile".into(), line: 0, at_eof: false });
        }
        let body = snapshot::decode(bytes, src).map_err(|message| ParseError {
            message,
            line: 0,
            at_eof: false,
        })?;
        let directive_strict = matches!(
            body.first(),
            Some(ast::Stmt::Expr(ast::Expr::Str(s))) if &**s == "use strict"
        );
        self.interp.strict = strict || directive_strict;
        let result = self.interp.run_program_parsed(&body);
        self.interp.run_agent_event_loop();
        match result {
            Ok(v) => Ok(Completion::Value(self.render(&v))),
            Err(thrown) => Ok(self.describe_throw(thrown)),
        }
    }

    /// Load and run an ahead-of-time compiled blob (see [`Precompiled`]): every script unit
    /// runs in order (like [`eval`](Engine::eval)), then the entry module, if the blob has
    /// one, is loaded and evaluated (like [`eval_module`](Engine::eval_module)). Every module
    /// unit is registered with the realm first, so imports between them resolve inside the
    /// blob; other specifiers go to the loader installed with
    /// [`set_module_loader`](Engine::set_module_loader) (install it before this call). A blob
    /// that fails validation (built by another lumen version, corrupt) is `Err(ParseError)`.
    pub fn load_precompiled(&mut self, blob: &Precompiled) -> Result<Completion, ParseError> {
        if native_ops::dynamic_code_disabled() {
            return Err(ParseError { message: "bytecode loading is unavailable in native execution".into(), line: 0, at_eof: false });
        }
        let bad = |message: String| ParseError {
            message,
            line: 0,
            at_eof: false,
        };
        let parsed = precompiled::parse_blob(blob).map_err(bad)?;
        precompiled::register_modules(&mut self.interp, &parsed);
        let mut last = Completion::Value(String::new());
        for unit in parsed.units.iter().filter(|u| u.kind == SourceKind::Script) {
            let body = unit
                .decode()
                .map_err(|e| bad(format!("{}: {e}", unit.key)))?;
            if memstats::enabled() {
                memstats::phase(&format!("  decoded {}", unit.key));
            }
            let directive_strict = matches!(
                body.first(),
                Some(ast::Stmt::Expr(ast::Expr::Str(s))) if &**s == "use strict"
            );
            self.interp.strict = directive_strict;
            let result = self.interp.run_program_named(&body, Some(&unit.key));
            self.interp.run_agent_event_loop();
            last = match result {
                Ok(v) => Completion::Value(self.render(&v)),
                Err(thrown) => return Ok(self.describe_throw(thrown)),
            };
        }
        if let Some(entry) = parsed.entry {
            let key = parsed.units[entry].key.clone();
            let result = self.interp.load_module(&key, "");
            self.interp.run_agent_event_loop();
            last = match result {
                Ok(_) => Completion::Value(String::new()),
                Err(a) => self.describe_throw(interpreter::abrupt_value(a)),
            };
        }
        Ok(last)
    }

    /// Load uploaded bytes without requiring static storage. Functions and registered
    /// modules retain the blob's backing until their last reference is released.
    pub fn load_precompiled_owned(&mut self, bytes: std::sync::Arc<[u8]>) -> Result<Completion, ParseError> {
        self.load_precompiled(&Precompiled::from_bytes(bytes))
    }

    /// Load an authenticated native image and retain its code with its functions.
    #[cfg(feature = "aot-native")]
    pub fn load_native_owned(
        &mut self,
        bytes: std::sync::Arc<[u8]>,
        signature: Option<[u8; 64]>,
        allowed_keys: &[[u8; 32]],
        allow_unsigned: bool,
    ) -> Result<Completion, String> {
        let value = native_aot::load_engine(&mut self.interp, bytes, signature, allowed_keys, allow_unsigned)?;
        Ok(Completion::Value(self.render(&value)))
    }

    /// Register a precompiled blob's modules with the realm without running anything, so a
    /// later `import` (static from another module, or dynamic) of an `aot:/…` key — or of a
    /// relative specifier from such a module — resolves inside the blob. Returns the entry
    /// module's key, if the blob has one.
    pub fn register_precompiled(
        &mut self,
        blob: &Precompiled,
    ) -> Result<Option<String>, ParseError> {
        let parsed = precompiled::parse_blob(blob).map_err(|message| ParseError {
            message,
            line: 0,
            at_eof: false,
        })?;
        precompiled::register_modules(&mut self.interp, &parsed);
        Ok(parsed.entry.map(|e| parsed.units[e].key.clone()))
    }

    /// Install a host module loader used by dynamic `import()` (and `eval_module`). `loader(specifier,
    /// referrer)` returns the imported module's `(canonical_key, source)`.
    pub fn set_module_loader(
        &mut self,
        loader: impl Fn(&str, &str) -> Option<(String, String)> + 'static,
    ) {
        self.interp.module_loader = Some(std::rc::Rc::new(
            move |s: &str, r: &str, _a: Option<&str>| loader(s, r),
        ));
    }

    /// [`Engine::set_module_loader`], with import attributes: the loader also receives the
    /// import's `with { type: ... }` attribute (`Some("json" | "text" | "bytes")` for the types
    /// the engine synthesizes). An attribute-aware host returns the RAW file contents for those —
    /// text/JSON per its own decoding policy, binary latin-1-decoded (one char per byte) — and
    /// the engine builds the synthetic module (default-exporting the parsed JSON, the string, or
    /// an immutable-backed `Uint8Array`) keyed separately from any ordinary module of the file.
    pub fn set_module_loader_attrs(
        &mut self,
        loader: impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> + 'static,
    ) {
        self.interp.module_loader = Some(std::rc::Rc::new(loader));
    }

    /// The default referrer for a bare `import()` in script code (so relative specifiers resolve).
    pub fn set_import_base(&mut self, base: &str) {
        self.interp.import_base = base.to_string();
    }

    /// Evaluate `src` as an ES module identified by `key`. `loader(specifier, referrer)` resolves an
    /// imported specifier to its `(canonical_key, source)`; it is consulted for every dependency.
    pub fn eval_module(
        &mut self,
        src: &str,
        key: &str,
        loader: impl Fn(&str, &str) -> Option<(String, String)> + 'static,
    ) -> Result<Completion, ParseError> {
        self.eval_module_attrs(src, key, move |s, r, _a| loader(s, r))
    }

    /// JSX module defaults for `.jsx`/`.tsx` and embedders using [`Engine::eval_module_jsx`].
    pub fn set_jsx_options(&mut self, options: JsxOptions) {
        self.interp.jsx_options = options;
    }

    pub fn jsx_options(&self) -> &JsxOptions {
        &self.interp.jsx_options
    }

    /// Resolve host JSX configuration per source file, before applying its pragmas.
    pub fn set_jsx_options_loader(&mut self, loader: impl Fn(&str, &JsxOptions) -> Result<JsxOptions, String> + 'static) {
        self.interp.jsx_options_loader = Some(std::rc::Rc::new(loader));
    }

    /// Evaluate JSX or TSX under an arbitrary module key (which need not have a file extension).
    pub fn eval_module_jsx(
        &mut self,
        src: &str,
        key: &str,
        typescript: bool,
        loader: impl Fn(&str, &str) -> Option<(String, String)> + 'static,
    ) -> Result<Completion, ParseError> {
        self.interp.jsx_module_keys.insert(key.to_string(), typescript);
        self.eval_module(src, key, loader)
    }

    /// [`Engine::eval_module`] with an attribute-aware loader (see
    /// [`Engine::set_module_loader_attrs`] for the contract).
    pub fn eval_module_attrs(
        &mut self,
        src: &str,
        key: &str,
        loader: impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> + 'static,
    ) -> Result<Completion, ParseError> {
        if native_ops::dynamic_code_disabled() {
            return Err(ParseError { message: "dynamic code is unavailable in native execution".into(), line: 0, at_eof: false });
        }
        self.interp.module_loader = Some(std::rc::Rc::new(loader));
        let result = self.interp.load_module(key, src);
        self.interp.run_agent_event_loop();
        Ok(match result {
            Ok(_) => Completion::Value(String::new()),
            Err(a) => self.describe_throw(interpreter::abrupt_value(a)),
        })
    }

    /// Select the execution tier (see [`bytecode::Tier`]). `Interp` never touches any codegen
    /// path; `Bytecode` — the default — compiles eligible functions after
    /// [`set_tier_threshold`](Engine::set_tier_threshold) calls.
    pub fn set_tier(&mut self, tier: bytecode::Tier) {
        self.interp.tier = tier;
    }

    /// Give an embedder a way to stop this realm from another thread. Once `flag` is set, the next
    /// safe point (a call, a loop turn) throws, every later one throws again, and no further
    /// promise reactions run.
    pub fn set_interrupt(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.interp.stop.set_interrupt(flag);
        self.interp.sync_regex_poll();
    }

    /// Cooperative time-slicing. When `flag` is observed set at a safe point (the same safe points
    /// as [`set_interrupt`](Engine::set_interrupt): calls, loop back-edges, bytecode and JIT loop
    /// polls, long native BigInt and regex work), Lumen clears `flag` and calls `hook` on the
    /// current native stack. The realm is quiescent during the call, so other engines may run on
    /// the same thread before `hook` returns (an embedder that switches stacks must give each
    /// engine its own thread-local block, and call [`set_thread_stack_bounds`] for a stack Lumen
    /// cannot query). When `hook` returns, execution continues where it stopped. A pending hard
    /// interrupt takes priority over a yield.
    pub fn set_yield_hook(
        &mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        hook: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) {
        self.interp.stop.set_yield_hook(flag, hook);
        self.interp.sync_regex_poll();
    }

    pub fn clear_yield_hook(&mut self) {
        self.interp.stop.clear_yield_hook();
        self.interp.sync_regex_poll();
    }

    /// Lower this realm's live-object ceiling (default [`interpreter::MAX_LIVE`]). With an
    /// interrupt set, crossing it terminates the realm (see [`Engine::heap_limit_hit`]); without
    /// one it is the usual catchable `RangeError`.
    pub fn set_live_object_limit(&mut self, limit: i64) {
        self.interp.live_limit = limit.clamp(interpreter::MIN_LIVE_LIMIT, interpreter::MAX_LIVE);
        self.interp.gc_next = self.interp.gc_next.min(self.interp.live_limit);
    }

    /// Cap the bytes the process holds through [`fastalloc::ClassAlloc`] (0 removes the cap).
    /// Past it, the next safe point (a call, a loop turn, every few thousand turns of a native
    /// loop, or a large allocation request) first collects, then throws a catchable
    /// `RangeError: JavaScript heap out of memory` and grants a headroom (an eighth of the
    /// limit, at least 16 MiB) for the handler; exceeding the headroom too throws again, or
    /// terminates the realm when an interrupt is set (see [`Engine::heap_limit_hit`]).
    ///
    /// The count is process-wide (every realm and thread allocating through `ClassAlloc`) and
    /// only works when `ClassAlloc` is the `#[global_allocator]`; see [`Engine::heap_bytes`].
    pub fn set_heap_limit(&mut self, bytes: usize) {
        self.interp.heap_limit = bytes;
        self.interp.heap_ceiling = bytes;
        self.interp.sync_regex_poll();
    }

    /// Make crossing the heap limit terminate the realm outright (see [`Engine::heap_limit_hit`])
    /// instead of throwing a catchable error first. Needs an interrupt flag on the realm.
    pub fn set_heap_limit_fatal(&mut self, fatal: bool) {
        self.interp.heap_fatal = fatal;
    }

    /// Bytes currently allocated through [`fastalloc::ClassAlloc`] by the whole process, or
    /// `None` when it is not the global allocator (a heap limit then has no effect).
    pub fn heap_bytes() -> Option<usize> {
        #[cfg(not(target_arch = "wasm32"))]
        return fastalloc::heap_bytes();
        #[cfg(target_arch = "wasm32")]
        None
    }

    /// Lower this realm's call-depth ceiling (default [`interpreter::MAX_EVAL_DEPTH`]); past it a
    /// call throws `RangeError: Maximum call stack size exceeded`. It cannot be raised above the
    /// default, which is what the engine's thread stacks are sized for.
    pub fn set_max_depth(&mut self, depth: u32) {
        self.interp.max_depth = depth.clamp(16, interpreter::MAX_EVAL_DEPTH);
        self.interp.depth_limit = 0;
    }

    /// Whether this realm has been terminated (interrupt or live-object ceiling).
    pub fn is_terminated(&self) -> bool {
        self.interp.terminating
    }

    /// Whether the termination came from the live-object ceiling or the heap limit.
    pub fn heap_limit_hit(&self) -> bool {
        self.interp.heap_limit_hit
    }

    /// The execution tier new calls are considered for (see [`set_tier`](Engine::set_tier)).
    pub fn tier(&self) -> bytecode::Tier {
        self.interp.tier
    }

    /// Calls before a function is considered for bytecode compilation (0 = immediately).
    pub fn set_tier_threshold(&mut self, threshold: u32) {
        self.interp.tier_threshold = threshold;
    }

    /// Drain anything written to `console.*` since the last call.
    pub fn take_console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.interp.console)
    }

    /// Node's uncaught report for a TypeScript SyntaxError (one with an
    /// `ERR_…_TYPESCRIPT_SYNTAX` `code` and a string `stack`).
    fn ts_uncaught_text(&mut self, thrown: &Value) -> Option<String> {
        if !matches!(thrown, Value::Obj(_)) {
            return None;
        }
        let code = match self.interp.get_member(thrown, "code") {
            Ok(v @ Value::Str(_)) => self.render(&v),
            _ => return None,
        };
        if !code.ends_with("_TYPESCRIPT_SYNTAX") {
            return None;
        }
        let stack = match self.interp.get_member(thrown, "stack") {
            Ok(v @ Value::Str(_)) => self.render(&v),
            _ => return None,
        };
        Some(typescript::node_uncaught_text(&stack, &code))
    }

    fn render(&mut self, v: &Value) -> String {
        self.interp
            .to_string(v)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn describe_throw(&mut self, thrown: Value) -> Completion {
        // A rejected TypeScript source reports as Node prints it (its `stack` and `code`),
        // with an empty name: callers print the message alone.
        if let Some(text) = self.ts_uncaught_text(&thrown) {
            return Completion::Throw {
                name: String::new(),
                message: text,
            };
        }
        // Pull the constructor name + message off an Error object; fall back to the rendered value.
        let name = match self.interp.get_member(&thrown, "name") {
            Ok(Value::Undefined) | Err(_) => {
                // No own/inherited `name` (e.g. Test262Error): use the constructor's name.
                match self.interp.get_member(&thrown, "constructor") {
                    Ok(ctor @ Value::Obj(_)) => match self.interp.get_member(&ctor, "name") {
                        Ok(Value::Undefined) | Err(_) => String::new(),
                        Ok(v) => self.render(&v),
                    },
                    _ => String::new(),
                }
            }
            Ok(v) => self.render(&v),
        };
        let message = match &thrown {
            Value::Obj(_) => match self.interp.get_member(&thrown, "message") {
                Ok(Value::Undefined) | Err(_) => String::new(),
                Ok(v) => self.render(&v),
            },
            other => self.render(other),
        };
        Completion::Throw { name, message }
    }
}

/// The curated embedder surface (`feature = "embed"`), for runtime layers (event loop, host
/// APIs) built on top of the engine. Gated because everything here is a semver commitment on a
/// published crate; it stabilizes together with the `lumen-host`/`lumen-runtime` crates.
#[cfg(feature = "embed")]
pub mod embed {
    pub use crate::host::{OpState, ResourceId, ResourceTable};
    /// The context a [`NativeFn`] receives: a curated view of the interpreter. Only the
    /// audited embedder-safe methods are `pub`; the rest of the interpreter is `pub(crate)`.
    pub use crate::interpreter::Interp as Ctx;
    pub use crate::interpreter::SharedBufferHandle;
    /// JS values. Matching/constructing the primitive variants is supported API; object
    /// internals stay opaque — an object handle is only usable through [`Ctx`] methods.
    /// A data-carrying native callable, unlike the bare-`fn` [`NativeFn`]. Register one with
    /// [`Ctx::new_native_fn`] when the host function must capture state (N-API callbacks).
    pub use crate::value::{NativeClosure, NativeFn, Value};

    // The JS host of `lumen-bind`, errors, promises and async work (see `embed_convert`).
    pub use crate::embed_convert::{
        class_name, js_name, ArgCx, AsyncHost, BigI64, BigU64, Completer, Deferred, JsArrayBuffer,
        JsFunction, JsHost, JsObject, OpError, OpInfo, OpResult, Promise, SendError, Settle, Slot,
        WeakValue,
    };
    /// A sync non-escaping callback argument (`#[op]` parameter type).
    pub use crate::sync_callbacks::{SyncFn, BUILTINS as SYNC_CALLBACK_BUILTINS};
    /// The language-neutral op error (maps to `TypeError` / `RangeError` / `Error`).
    pub use lumen_common::native::{ErrorKind as NativeErrorKind, NativeError, NativeResult};
    /// The binding framework (declare natives with `lumen_bind::{op, class, methods, module}`).
    pub use lumen_bind::{self as bind, State, This};
}

/// Embedder methods (`feature = "embed"`). Native functions registered here are bare `fn`
/// pointers (they cannot capture); Rust state lives in [`embed::OpState`], reached through the
/// `&mut Ctx` argument.
#[cfg(feature = "embed")]
impl Engine {
    /// Direct access to the native-function context (also where [`embed::OpState`] lives, via
    /// [`embed::Ctx::op_state`]).
    pub fn ctx(&mut self) -> &mut embed::Ctx {
        &mut self.interp
    }

    /// Load compiled-in bootstrap glue with the baseline, import-free glue contract.
    #[cfg(feature = "aot-native")]
    pub fn load_native_glue_value(&mut self, bytes: &'static [u8]) -> Result<embed::Value, String> {
        native_aot::load_static_glue_engine(&mut self.interp, bytes)
    }

    #[cfg(feature = "aot-native")]
    pub fn validate_native_install(&self, bytes: &[u8], additional_modules: &[&str]) -> Result<(), String> {
        native_aot::validate_install(&self.interp, bytes, additional_modules)
    }

    /// Native entry value without rendering, so an embedder can await its promise.
    #[cfg(feature = "aot-native")]
    pub fn load_native_value_owned(
        &mut self,
        bytes: std::sync::Arc<[u8]>,
        signature: Option<[u8; 64]>,
        allowed_keys: &[[u8; 32]],
        allow_unsigned: bool,
    ) -> Result<embed::Value, String> {
        native_aot::load_engine(&mut self.interp, bytes, signature, allowed_keys, allow_unsigned)
    }

    /// Load process-linked code and its writable GOT without copying executable pages.
    ///
    /// # Safety
    /// Code and GOT must remain valid for every resulting callable. GOT publication
    /// must be serialized with all other loads of this same linked image.
    #[cfg(feature = "aot-native")]
    pub unsafe fn load_native_linked_value_owned(
        &mut self, bytes: std::sync::Arc<[u8]>, code: *const u8, code_len: usize,
        got: *mut usize, got_len: usize,
    ) -> Result<embed::Value, String> {
        unsafe { native_aot::load_linked_engine(&mut self.interp, bytes, code, code_len, got, got_len) }
    }

    /// The realm's global object — the root from which an embedder reaches user-defined JS
    /// (e.g. `ctx().get_member(&engine.global_this(), "myCallback")`).
    pub fn global_this(&self) -> embed::Value {
        Value::Obj(self.interp.global.clone())
    }

    /// Register an embedder-provided module namespace before loading a native image.
    /// Its identity is covered by the target's built-in module table hash.
    pub fn register_native_module(
        &mut self,
        specifier: impl Into<String>,
        namespace: embed::Value,
    ) -> Result<(), &'static str> {
        let specifier = specifier.into();
        if specifier.is_empty() || !matches!(&namespace, Value::Obj(_)) {
            return Err("native module requires a name and namespace object");
        }
        let catalog = target::host().builtin_modules_hash;
        if catalog == target::EMPTY_BUILTIN_MODULES_HASH
            || (catalog == target::PARALLEL_BUILTIN_MODULES_HASH
                && specifier != "lumen:parallel")
        {
            return Err("module is absent from the built-in native catalog");
        }
        if self.interp.modules.contains_key(&specifier) {
            return Err("native module already registered");
        }
        self.interp.native_module_names.insert(specifier.clone());
        self.interp.modules.insert(specifier, namespace);
        Ok(())
    }

    /// Register one entry from an embedder-defined native binding catalog.
    ///
    /// # Safety
    /// `address` must be a live function with the ABI identified by
    /// `signature_hash` for every image loaded by this realm.
    pub unsafe fn register_native_binding(
        &mut self,
        module: impl Into<String>,
        name: impl Into<String>,
        signature_hash: u64,
        address: usize,
    ) -> Result<(), &'static str> {
        let module = module.into();
        let name = name.into();
        if module.is_empty() || name.is_empty() || signature_hash == 0 || address == 0 {
            return Err("invalid native binding registration");
        }
        let catalog = target::host().builtin_modules_hash;
        if catalog == target::EMPTY_BUILTIN_MODULES_HASH
            || catalog == target::PARALLEL_BUILTIN_MODULES_HASH
        {
            return Err("native binding is absent from the built-in catalog");
        }
        if self.interp.native_bindings.contains_key(&(module.clone(), name.clone())) {
            return Err("native binding already registered");
        }
        self.interp.native_bindings.insert((module, name), (signature_hash, address));
        Ok(())
    }

    /// [`eval`](Engine::eval), but the completion comes back as real values (`Err` = the
    /// thrown value) and NO microtask checkpoint runs — the caller owns the event loop and
    /// decides when jobs fire (a REPL runs its runtime to quiescence between inputs). The
    /// spec's EMPTY completion (a value-less final statement) is lowered to `undefined`.
    pub fn eval_value(
        &mut self,
        src: &str,
    ) -> Result<Result<embed::Value, embed::Value>, ParseError> {
        if native_ops::dynamic_code_disabled() {
            return Err(ParseError { message: "dynamic code is unavailable in native execution".into(), line: 0, at_eof: false });
        }
        let body = parser::parse_script(src, false).map_err(|e| ParseError {
            message: e.message,
            line: e.line,
            at_eof: e.at_eof,
        })?;
        let directive_strict = matches!(
            body.first(),
            Some(ast::Stmt::Expr(ast::Expr::Str(s))) if &**s == "use strict"
        );
        self.interp.strict = directive_strict;
        Ok(match self.interp.run_program_parsed(&body) {
            Ok(Value::Empty) => Ok(Value::Undefined),
            result => result,
        })
    }

    /// Define `globalThis.<name>` as a native function (non-enumerable, like built-ins).
    pub fn define_global(&mut self, name: &str, len: usize, f: embed::NativeFn) {
        let global = self.interp.global.clone();
        self.interp.def_method(&global, name, len, f);
    }

    /// Define `globalThis.<name>` as a namespace object (like `Math`) with the given
    /// `(name, arity, fn)` native methods.
    pub fn define_namespace(&mut self, name: &str, ops: &[(&str, usize, embed::NativeFn)]) {
        let ns = self.interp.new_object();
        for (op, len, f) in ops {
            self.interp.def_method(&ns, op, *len, *f);
        }
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(name, crate::value::Property::builtin(Value::Obj(ns)));
    }

    /// Call a JS function value; `Err` is the thrown value. This is how the runtime's event
    /// loop re-enters the engine to fire a timer/IO callback, so it must work on every
    /// execution tier, not just the interpreter.
    pub fn call_function(
        &mut self,
        func: &embed::Value,
        this: embed::Value,
        args: &[embed::Value],
    ) -> Result<embed::Value, embed::Value> {
        self.interp
            .call(func.clone(), this, args)
            .map_err(interpreter::abrupt_value)
    }

    /// Drain the microtask (promise-reaction) queue to quiescence, then compact the string
    /// buffers only views of them still use (`lstr::compact_views`: safe here, with no JS or
    /// native frame running that could hold a borrowed string).
    pub fn run_microtasks(&mut self) {
        self.interp.drain_microtasks();
        // A parked OS coroutine can retain a Rust native stack frame borrowing view
        // bytes. Its driver is active, but moving those bytes would invalidate the borrow.
        // This registry count includes both generators and ordinary async coroutines
        // retained by promise data; no heap scan or generator-only ownership assumption.
        if crate::lstr::views_can_compact() { crate::lstr::compact_views(); }
    }

    /// `(roots, views, root bytes)` of the string-view registry (see `lstr`), for tests and
    /// memory diagnostics.
    pub fn string_view_stats(&self) -> (usize, usize, usize) {
        crate::lstr::view_stats()
    }

    /// Drain and return the reasons of promises rejected without a handler (after a microtask
    /// checkpoint, these are genuine unhandled rejections). The runtime reports them; the bare
    /// engine ignores them, so test262 semantics are unaffected.
    pub fn take_unhandled_rejections(&mut self) -> Vec<embed::Value> {
        self.take_unhandled_rejections_full()
            .into_iter()
            .map(|(_promise, reason)| reason)
            .collect()
    }

    /// [`Engine::take_unhandled_rejections`], keeping the promise alongside each reason (what a
    /// global `unhandledrejection` handler receives as `event.promise` / `event.reason`).
    pub fn take_unhandled_rejections_full(&mut self) -> Vec<(embed::Value, embed::Value)> {
        if self.interp.unhandled_rejections.is_empty() {
            return Vec::new();
        }
        let mut rejections: Vec<_> = std::mem::take(&mut self.interp.unhandled_rejections)
            .into_values()
            .collect();
        rejections.sort_unstable_by_key(|r| r.2);
        rejections
            .into_iter()
            .map(|(promise, reason, _)| (promise, reason))
            .collect()
    }

    /// Start recording rejections that get a handler after being reported unhandled (see
    /// [`Engine::take_late_handled_rejections`]).
    pub fn track_late_handled_rejections(&mut self) {
        self.interp.late_handled_rejections.get_or_insert_with(Vec::new);
    }

    /// Promises reported by [`Engine::take_unhandled_rejections_full`] that have since been
    /// handled (Node's `rejectionHandled`), in handling order.
    pub fn take_late_handled_rejections(&mut self) -> Vec<embed::Value> {
        match self.interp.late_handled_rejections.as_mut() {
            Some(list) => std::mem::take(list),
            None => Vec::new(),
        }
    }

    /// Report `queueMicrotask` callback throws through [`Engine::take_task_errors`] instead of
    /// as unhandled rejections.
    pub fn report_task_errors(&mut self) {
        self.interp.task_errors.get_or_insert_with(Vec::new);
    }

    /// Errors thrown by `queueMicrotask` callbacks since the last call.
    pub fn take_task_errors(&mut self) -> Vec<embed::Value> {
        match self.interp.task_errors.as_mut() {
            Some(list) => std::mem::take(list),
            None => Vec::new(),
        }
    }

    /// Whether promise-reaction jobs are queued (the loop uses this to decide when a turn is
    /// really over).
    pub fn has_pending_jobs(&self) -> bool {
        !self.interp.microtasks.is_empty()
    }

    /// Run a single queued job; `false` when the queue was empty.
    pub fn run_one_job(&mut self) -> bool {
        match self.interp.microtasks.pop_front() {
            Some(job) => {
                self.interp.run_job(job);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
