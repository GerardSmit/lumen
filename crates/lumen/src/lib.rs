//! lumen — a from-scratch JavaScript engine (std-only, no dependencies).
//!
//! lumen is the eventual in-house replacement for the V8 backend in the `js` crate. Today it is a
//! tree-walking interpreter covering the ECMAScript language core, driven by the tc39/test262
//! conformance suite (see `crates/test262-runner`). It deliberately implements a growing *subset* —
//! the test262 score is the roadmap.
//!
//! ## Shape
//! - [`lexer`] tokenizes, [`parser`] builds the [`ast`], [`interpreter`] + `eval` walk it.
//! - [`value`] is the prototype-based object model (`Rc<RefCell<Object>>`, reference-counted — no
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
mod bigint;
mod builtins;
pub mod bytebuf;
pub mod bytecode;
mod coroutine;
/// Typed Rust <-> JS conversions and the runtime of the binding macros (see [`embed`]).
#[cfg(feature = "embed")]
mod embed_convert;
mod eval;
/// The engine's size-class caching allocator — allocation-bound workloads (one refcounted box
/// per JS object/scope) run 15-30% faster than on the system allocator. NOT registered here: a
/// library must not preempt an embedder's `#[global_allocator]` (the test262 runner caps
/// worker allocations with its own). Binaries opt in:
/// `#[global_allocator] static A: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;`
#[cfg(not(target_arch = "wasm32"))]
pub mod fastalloc;
mod fasthash;
mod host;
mod interpreter;
#[cfg(feature = "intl")]
mod intl;
mod jit_ir;
mod jstr;
mod lexer;
mod lstr;
mod modules;
#[doc(hidden)]
pub use modules::load_stats;
#[cfg(feature = "intl")]
mod numbering;
mod lzh;
/// Opt-in memory accounting (`LUMEN_MEM_STATS=1`).
pub mod memstats;
mod parser;
pub mod precompiled;
mod regex;
mod regex_emoji;
mod regex_fold;
mod snapshot;
mod sync_callbacks;
mod temporal;
mod token;
mod tz;
pub mod typescript;
#[rustfmt::skip]
mod tzdata;
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
mod units;
mod unicode_norm;
mod unicode_norm_impl;
mod unicode_props;
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
    pub use crate::parser::{parse_module, parse_module_ts, parse_script};
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

/// A JS string's text as well-formed UTF-8 (lone surrogates become U+FFFD): what encoders
/// such as `TextEncoder` write, as opposed to the engine's internal form.
pub fn well_formed_utf8(s: &str) -> std::borrow::Cow<'_, str> {
    jstr::well_formed(s)
}

/// Ahead-of-time compilation (see [`Engine::load_precompiled`] and the `lumen-aot` crate):
/// [`precompile`] / [`precompiled::PrecompileBundle`] run at build time and produce a
/// source-free blob; [`Precompiled`] wraps one linked into the binary.
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

/// A JavaScript engine instance: one realm (global object + intrinsics) that persists across
/// [`eval`](Engine::eval) calls.
pub struct Engine {
    interp: Interp,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        interpreter::sym_for_reset();
        let _mem = memstats::enter(memstats::Cat::Builtins);
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
        let bad = |message: String| ParseError {
            message,
            line: 0,
            at_eof: false,
        };
        let parsed = precompiled::parse(blob.as_bytes()).map_err(bad)?;
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

    /// Register a precompiled blob's modules with the realm without running anything, so a
    /// later `import` (static from another module, or dynamic) of an `aot:/…` key — or of a
    /// relative specifier from such a module — resolves inside the blob. Returns the entry
    /// module's key, if the blob has one.
    pub fn register_precompiled(
        &mut self,
        blob: &Precompiled,
    ) -> Result<Option<String>, ParseError> {
        let parsed = precompiled::parse(blob.as_bytes()).map_err(|message| ParseError {
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

    /// [`Engine::eval_module`] with an attribute-aware loader (see
    /// [`Engine::set_module_loader_attrs`] for the contract).
    pub fn eval_module_attrs(
        &mut self,
        src: &str,
        key: &str,
        loader: impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> + 'static,
    ) -> Result<Completion, ParseError> {
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
        self.interp.interrupt = Some(flag);
    }

    /// Lower this realm's live-object ceiling (default [`interpreter::MAX_LIVE`]). With an
    /// interrupt set, crossing it terminates the realm (see [`Engine::heap_limit_hit`]); without
    /// one it is the usual catchable `RangeError`.
    pub fn set_live_object_limit(&mut self, limit: i64) {
        self.interp.live_limit = limit.clamp(interpreter::GC_TRIGGER, interpreter::MAX_LIVE);
        self.interp.gc_next = self.interp.gc_next.min(self.interp.live_limit);
    }

    /// Whether this realm has been terminated (interrupt or live-object ceiling).
    pub fn is_terminated(&self) -> bool {
        self.interp.terminating
    }

    /// Whether the termination came from the live-object ceiling.
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
    /// JS values. Matching/constructing the primitive variants is supported API; object
    /// internals stay opaque — an object handle is only usable through [`Ctx`] methods.
    /// A data-carrying native callable, unlike the bare-`fn` [`NativeFn`]. Register one with
    /// [`Ctx::new_native_fn`] when the host function must capture state (N-API callbacks).
    pub use crate::value::{NativeClosure, NativeFn, Value};

    // Typed bindings: conversion traits, op/class descriptors, promises (see `embed_convert`).
    pub use crate::embed_convert::{
        ArgCx, ArrayElem, AsyncHost, BigI64, BigU64, Class, ClassDesc, Completer, CtorReturn,
        Deferred, FastKind, FastPtr, FastSig, FromJs, IntoJs, JsArrayBuffer, JsFunction, JsObject,
        MemberDesc, MemberKind, OpDesc, OpError, OpResult, Promise, SendError, Settle, Slot, State,
        This,
    };
    /// A sync non-escaping callback argument (`#[op]` parameter type).
    pub use crate::sync_callbacks::{SyncFn, BUILTINS as SYNC_CALLBACK_BUILTINS};
    #[doc(hidden)]
    pub use crate::embed_convert::private as __private;
    /// The binding macros (feature `macros`), also at the crate root.
    #[cfg(feature = "macros")]
    pub use lumen_macros::{class, methods, op};
}

/// `#[op]` / `#[class]` / `#[methods]` (feature `macros`): see `lumen_macros`.
#[cfg(feature = "macros")]
pub use lumen_macros::{class, methods, op};

/// `lumen::ops![a, b, path::c]` — the `&'static OpDesc` list of `#[op]` fns, for
/// [`Engine::define_ops`].
#[cfg(feature = "embed")]
#[macro_export]
macro_rules! ops {
    ($($($p:ident)::+),* $(,)?) => {
        &[$(&$($p)::+::DESC),*]
    };
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

    /// The realm's global object — the root from which an embedder reaches user-defined JS
    /// (e.g. `ctx().get_member(&engine.global_this(), "myCallback")`).
    pub fn global_this(&self) -> embed::Value {
        Value::Obj(self.interp.global.clone())
    }

    /// [`eval`](Engine::eval), but the completion comes back as real values (`Err` = the
    /// thrown value) and NO microtask checkpoint runs — the caller owns the event loop and
    /// decides when jobs fire (a REPL runs its runtime to quiescence between inputs). The
    /// spec's EMPTY completion (a value-less final statement) is lowered to `undefined`.
    pub fn eval_value(
        &mut self,
        src: &str,
    ) -> Result<Result<embed::Value, embed::Value>, ParseError> {
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
        crate::lstr::compact_views();
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
        std::mem::take(&mut self.interp.unhandled_rejections)
            .into_values()
            .collect()
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
