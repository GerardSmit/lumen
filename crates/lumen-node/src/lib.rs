//! lumen-node — `node:` builtin compatibility.
//!
//! The interesting parts are JS (see `src/js/`): the CommonJS `require` implementation with
//! `node_modules` resolution and `package.json` `main`/`exports`, `Buffer`, `node:path`, and
//! `node:fs`/`node:os` shims. Rust backs only what JS can't reach: filesystem *classification*
//! for the resolver (is-file / is-dir, distinct from the fs ops' read/write) and OS facts.
//!
//! Scope, stated honestly (checklist — [ ] = not yet):
//! - [x] CommonJS `require`: core modules, relative/absolute, `node_modules` walk, the module
//!   wrapper via `new Function`, `require.cache`/`require.resolve`/`require.main`, `.js`/
//!   `.json`/`.cjs`
//! - [x] `package.json` `main`; exact/single-star exports, ordered
//!   `require`/`node`/`default` conditions and array targets — [ ] full export validation
//! - [x] `node:sqlite` core synchronous database/statements over native SQLite, with native
//!   transaction/location/column metadata and connection flags — [ ] authorizers, user
//!   functions, sessions, backup, extensions, custom limits, automatic statement finalization
//! - [x] `node:path` (posix + win32), `node:os`, `node:fs` (sync + callback + `.promises`)
//! - [x] `Buffer` (from/alloc/concat, utf8·hex·base64·latin1·ascii, slice/write/compare, the
//!   common read/write-int accessors) — [ ] every codec + accessor variant
//! - [x] `global`, `__dirname`/`__filename` (per module)
//! - [x] native addons: `require('./x.node')` dlopens the library and runs its N-API
//!   registration (see `napi.rs` for the implemented `napi_*` surface and `dylib.rs` for the
//!   dependency-free loader) — [ ] the full ~150-function N-API, references, threadsafe funcs
//! - [ ] ESM `import` of `node:` specifiers (this is the CommonJS surface); `require('esm')`

use std::path::Path;

use lumen_host::{Ctx, Extension, Value};

#[cfg(feature = "bun")]
mod bunhash;
#[cfg(not(target_arch = "wasm32"))]
mod child;
mod codec;
mod crypto;
#[cfg(not(target_arch = "wasm32"))]
mod dns;
#[cfg(not(target_arch = "wasm32"))]
mod dylib;
#[cfg(all(feature = "bun", not(target_arch = "wasm32")))]
mod ffi;
#[cfg(feature = "compiler")]
mod glue_dev;
#[cfg(windows)]
mod win_pipe;
#[cfg(windows)]
mod win_spawn;
use lumen_common::hash;
#[cfg(target_arch = "wasm32")]
mod browser;
mod fsb;
#[cfg(not(target_arch = "wasm32"))]
mod napi;
mod native;
#[cfg(not(target_arch = "wasm32"))]
mod net;
mod oscon;
mod password;
mod pathops;
mod signals;
#[cfg(all(feature = "bun", not(target_arch = "wasm32")))]
mod sqlite;
#[cfg(not(target_arch = "wasm32"))]
mod tls;
mod vm_context;
#[cfg(not(target_arch = "wasm32"))]
mod vm_timeout;
mod zlib;
#[cfg(target_arch = "wasm32")]
use browser::{child, dns, napi, net, tls, vm_timeout};
#[cfg(all(feature = "bun", target_arch = "wasm32"))]
use browser::{ffi, sqlite};

pub use signals::SigintBreak;

pub fn extension() -> Extension {
    #[cfg(feature = "compiler")]
    let dev_glue = glue_dev::source();
    #[cfg(not(feature = "compiler"))]
    let dev_glue = None;
    Extension {
        name: "node",
        modules: &[
            lumen_host::namespace::<oscon::Module>,
            lumen_host::namespace::<node_bindings::Module>,
            lumen_host::namespace::<pathops::Module>,
            lumen_host::namespace::<signals::Module>,
            lumen_host::namespace::<napi::Module>,
            lumen_host::namespace::<os_bindings::Module>,
            lumen_host::namespace::<child::Module>,
            lumen_host::namespace::<vm_timeout::Module>,
            lumen_host::namespace::<vm_context::Module>,
            lumen_host::namespace::<net::Module>,
            lumen_host::namespace::<net::UdpModule>,
            lumen_host::namespace::<tls::Module>,
            lumen_host::namespace::<password::Module>,
            lumen_host::namespace::<dns::Module>,
            lumen_host::namespace::<zlib::Module>,
            #[cfg(feature = "bun")]
            lumen_host::namespace::<sqlite::Module>,
            #[cfg(feature = "bun")]
            lumen_host::namespace::<bunhash::Module>,
            #[cfg(feature = "bun")]
            lumen_host::namespace::<ffi::Module>,
        ],
        state_init: Some(|state: &mut lumen_host::OpState| {
            state.put(child::ChildRegistry::default());
            state.put(net::NetRegistry::default());
            state.put(net::DgramRegistry::default());
            state.put(tls::TlsRegistry::default());
            state.put(zlib::ZlibHandles::default());
        }),
        js_init: dev_glue,
        js_init_snapshot: if dev_glue.is_some() {
            None
        } else {
            Some(JS_GLUE_AOT)
        },
    }
}

// Assembled by build.rs from src/js/*.js (single source of truth) and precompiled there to an
// ahead-of-time blob (AST, bytecode, compressed function text), loaded at boot.
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node_glue.aot"));

include!(concat!(env!("OUT_DIR"), "/esm_exports.rs"));

/// `__node`: the realm facts, module-loading and inspection primitives the glue reads. The
/// remaining raw ops of this namespace are installed beside it by [`install_node`].
#[lumen_bind::module(name = "__node")]
mod node_bindings {
    use super::*;
    use lumen::embed::OpError;
    use lumen_bind::{NativeError, NativeResult};

    /// `() -> string | undefined` — the embedded realm's working directory, or `undefined` when the
    /// runtime owns its process (the OS cwd is then the right base and paths stay as written).
    #[op(name = "realmCwd")]
    pub fn realm_cwd(ctx: &mut Ctx) -> Value {
        ctx.op_state()
            .get::<lumen_host::RealmProcess>()
            .map(|realm| Value::from_string(realm.cwd.to_string_lossy().into_owned()))
            .unwrap_or(Value::Undefined)
    }

    /// `__node.fsBinding()`: a fresh object holding every fs op (called once by fs.js).
    #[op(name = "fsBinding")]
    pub fn fs_binding(ctx: &mut Ctx) -> Result<Value, Value> {
        fsb::op_fs_binding(ctx, Value::Undefined, &[])
    }

    /// `__node.cryptoBinding()`: the object holding every crypto op (called once by preamble.js).
    #[op(name = "cryptoBinding")]
    pub fn crypto_binding(ctx: &mut Ctx) -> Result<Value, Value> {
        crypto::op_crypto_binding(ctx, Value::Undefined, &[])
    }

    #[op(coerce, name = "isFile")]
    pub fn is_file(path: &str) -> bool {
        use lumen_host::sysfs::PathExt;
        Path::new(path).fs_is_file()
    }

    #[op(coerce, name = "isDir")]
    pub fn is_dir(path: &str) -> bool {
        lumen_host::sysfs::is_dir(path)
    }

    /// Read a module/JSON source as text; a miss is an error the resolver turns into
    /// MODULE_NOT_FOUND context.
    #[op(coerce, name = "readText")]
    pub fn read_text(path: &str) -> NativeResult<String> {
        match lumen_host::sysfs::read_to_string(path) {
            Ok(s) => Ok(crate::codec::canonical(s)),
            Err(e) => Err(NativeError::runtime(format!("cannot read '{path}': {e}"))),
        }
    }

    /// Read a file as raw bytes (a Uint8Array), for `fs.readFileSync` without an encoding — the text
    /// path corrupts binary. Errors carry the errno `code` Node users switch on.
    #[op(coerce, name = "readBytes")]
    pub fn read_bytes(path: &str) -> NativeResult<Vec<u8>> {
        lumen_host::sysfs::read(path).map_err(|e| {
            let code = match e.kind() {
                std::io::ErrorKind::NotFound => "ENOENT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                _ => "EIO",
            };
            NativeError::runtime(format!("cannot read '{path}': {e}")).with_code(code)
        })
    }

    /// `(source)` — the source with a leading `#!` line marker turned into `//`, keeping every
    /// offset. Done natively: any JS string method on a large source materializes a UTF-16 copy.
    #[op(name = "stripShebang")]
    pub fn strip_shebang(source: Value) -> Value {
        if let Value::Str(s) = &source {
            let text = s.as_str();
            let body = text.strip_prefix('\u{feff}').unwrap_or(text);
            if let Some(rest) = body.strip_prefix("#!") {
                return Value::from_string(format!("//{rest}"));
            }
            if body.len() != text.len() {
                return Value::from_string(body.to_string());
            }
        }
        source
    }

    /// Canonicalize (resolve symlinks) for the module cache key; falls back to the input when the
    /// path doesn't exist yet (matching how the JS resolver probes candidates).
    #[op(coerce)]
    pub fn realpath(path: &str) -> String {
        match lumen_host::canonicalize(path) {
            Ok(c) => c.to_string_lossy().into_owned(),
            Err(_) => path.to_string(),
        }
    }

    #[op(name = "isProxy")]
    pub fn is_proxy(ctx: &mut Ctx, value: Value) -> bool {
        ctx.is_proxy_value(&value)
    }

    /// `(fn, filename)` — name a CommonJS module wrapper's source for stack traces (see
    /// `Ctx::name_function_source`).
    #[op(name = "nameSource")]
    pub fn name_source(ctx: &mut Ctx, function: Value, filename: Value) -> bool {
        match filename {
            Value::Str(s) => ctx.name_function_source(&function, s.as_ref()),
            _ => false,
        }
    }

    /// `(promise)` — `[status, value]` (0 pending, 1 fulfilled, 2 rejected) for util.inspect, or
    /// `undefined` for a non-promise.
    #[op(name = "promiseState")]
    pub fn promise_state(ctx: &mut Ctx, promise: Value) -> Value {
        match ctx.promise_state_for_host(&promise) {
            Some((status, value)) => ctx.make_array(vec![Value::Num(status as f64), value]),
            None => Value::Undefined,
        }
    }

    /// `(proxy)` — `[target, handler]` for util.inspect's `showProxy`, or `undefined`.
    #[op(name = "proxyParts")]
    pub fn proxy_parts(ctx: &mut Ctx, proxy: Value) -> Value {
        match ctx.proxy_parts_for_host(&proxy) {
            Some((target, handler)) => ctx.make_array(vec![target, handler]),
            None => Value::Undefined,
        }
    }

    /// `(iterator, isKeyValue)` — V8's `previewEntries`: a Map/Set iterator's remaining entries
    /// without advancing it; `[entries, isKeyValue]` when `isKeyValue` is true, else `entries`.
    #[op(name = "previewEntries")]
    pub fn preview_entries(ctx: &mut Ctx, iterator: Value, is_key_value: Option<Value>) -> Value {
        let Some((entries, key_value)) = ctx.preview_entries_for_host(&iterator) else {
            return Value::Undefined;
        };
        let list = ctx.make_array(entries);
        if matches!(is_key_value, Some(Value::Bool(true))) {
            ctx.make_array(vec![list, Value::Bool(key_value)])
        } else {
            list
        }
    }

    #[op(name = "collectGarbage")]
    pub fn collect_garbage(ctx: &mut Ctx) -> f64 {
        ctx.collect_garbage_for_host() as f64
    }

    /// `(flags)` — apply the V8 flags lumen implements from a `v8.setFlagsFromString` string
    /// (`--allow-natives-syntax` and its `--no` form); the rest have no lumen counterpart.
    #[op(name = "v8SetFlags")]
    pub fn v8_set_flags(ctx: &mut Ctx, flags: Value) {
        let Value::Str(flags) = flags else {
            return;
        };
        for flag in flags.to_string().split_whitespace() {
            let flag = flag.trim_start_matches('-').replace('_', "-");
            let (on, name) = match flag.strip_prefix("no-").or_else(|| flag.strip_prefix("no")) {
                Some(rest) if rest == "allow-natives-syntax" => (false, rest),
                _ => (true, flag.as_str()),
            };
            if name == "allow-natives-syntax" {
                ctx.set_natives_syntax(on);
            }
        }
    }

    /// `(path?)` — a V8 heap snapshot of this realm's heap: written to `path`, or returned as a
    /// `Uint8Array` of its JSON.
    #[op(name = "heapSnapshot")]
    pub fn heap_snapshot(ctx: &mut Ctx, path: Option<Value>) -> NativeResult<Option<Vec<u8>>> {
        match path {
            Some(Value::Str(path)) => {
                let path = path.to_string();
                let written = std::fs::File::create(&path).and_then(|f| {
                    let mut out = std::io::BufWriter::with_capacity(64 * 1024, f);
                    ctx.write_heap_snapshot(&mut out)
                });
                written.map(|()| None).map_err(|e| {
                    NativeError::runtime(format!("cannot write heap snapshot '{path}': {e}"))
                })
            }
            _ => {
                let mut out = Vec::new();
                ctx.write_heap_snapshot(&mut out)
                    .map_err(|e| NativeError::runtime(format!("heap snapshot: {e}")))?;
                Ok(Some(out))
            }
        }
    }

    /// `(times, threadId, dir?)` — write a heap snapshot the first `times` times the realm reaches its
    /// memory ceiling (Node's `v8.setHeapSnapshotNearHeapLimit`); `0` stops. The realm then runs on
    /// with a slightly higher ceiling, and the last crossing is the limit for good.
    #[op(name = "setNearHeapLimit")]
    pub fn set_near_heap_limit(ctx: &mut Ctx, times: Value, thread_id: Value, dir: Option<Value>) {
        let times = times.as_num_opt().unwrap_or(0.0).max(0.0) as u32;
        let thread_id = thread_id.as_num_opt().unwrap_or(0.0) as u32;
        let dir = match dir {
            Some(Value::Str(d)) if !d.to_string().is_empty() => {
                Some(std::path::PathBuf::from(d.to_string()))
            }
            _ => None,
        };
        ctx.set_near_limit_hook(
            times,
            Box::new(move |interp| {
                static SEQUENCE: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let name = format!(
                    "Heap.{}.{}.{thread_id}.{seq:03}.heapsnapshot",
                    local_stamp(),
                    std::process::id()
                );
                let path = dir
                    .as_ref()
                    .map_or_else(|| std::path::PathBuf::from(&name), |d| d.join(&name));
                let _ = std::fs::File::create(&path).and_then(|f| {
                    let mut out = std::io::BufWriter::with_capacity(64 * 1024, f);
                    interp.write_heap_snapshot(&mut out)
                });
            }),
        );
    }

    /// `[nodeStart, v8Start, environment, bootstrapComplete, loopStart, loopExit, idleTime]` in
    /// milliseconds on the `performance.now()` clock (-1: not reached yet).
    #[op(name = "perfTiming")]
    pub fn perf_timing() -> Vec<f64> {
        lumen_host::perf::snapshot().into_iter().collect()
    }

    /// `[liveHeapObjects, arrayBufferBytes]` for `process.memoryUsage()`.
    #[op(name = "memoryStats")]
    pub fn memory_stats(ctx: &mut Ctx) -> Vec<f64> {
        vec![
            ctx.live_object_count() as f64,
            lumen_common::buffer::tracked_bytes() as f64,
        ]
    }

    /// `(callback | null)` — log every collection and queue `callback` as a microtask after it;
    /// `null` stops.
    #[op(name = "gcObserve")]
    pub fn gc_observe(ctx: &mut Ctx, callback: Value) {
        if callback.is_callable() {
            let mut observer = lumen::gc_log::GcObserver::new(callback);
            observer.epoch_ms = lumen_host::perf::now_ms();
            ctx.op_state().put(observer);
        } else {
            ctx.op_state().take::<lumen::gc_log::GcObserver>();
        }
    }

    /// `()` — the logged collections as a flat `[startTime, duration, forced, ...]` array (ms, on
    /// the `performance.now()` clock), clearing the log.
    #[op(name = "gcTake")]
    pub fn gc_take(ctx: &mut Ctx) -> Value {
        let (events, base_ms) = match ctx.op_state().get_mut::<lumen::gc_log::GcObserver>() {
            Some(observer) => {
                observer.queued = false;
                (std::mem::take(&mut observer.events), observer.epoch_ms)
            }
            None => (Vec::new(), 0.0),
        };
        let mut flat = Vec::with_capacity(events.len() * 3);
        for event in events {
            flat.push(Value::Num(base_ms + event.start.as_secs_f64() * 1000.0));
            flat.push(Value::Num(event.duration.as_secs_f64() * 1000.0));
            flat.push(Value::Bool(event.forced));
        }
        ctx.make_array(flat)
    }

    /// `()` — the current async context value (see `Interp::async_context`).
    #[op(name = "asyncContextGet")]
    pub fn async_context_get(ctx: &mut Ctx) -> Value {
        ctx.async_context()
    }

    /// `(context)` — make `context` current, returning the previous one so the caller can restore it.
    #[op(name = "asyncContextSet")]
    pub fn async_context_set(ctx: &mut Ctx, context: Value) -> Value {
        ctx.set_async_context(context)
    }

    /// `(init, before, after, settled)` — install V8-style promise hooks (`v8.promiseHooks`,
    /// async_hooks); all four non-callable removes them.
    #[op(name = "setPromiseHooks")]
    pub fn set_promise_hooks(
        ctx: &mut Ctx,
        init: Value,
        before: Value,
        after: Value,
        settled: Value,
    ) {
        ctx.set_promise_hooks(Some([init, before, after, settled]));
    }

    /// `(error)` — report `error` as an uncaught exception after the current microtask checkpoint
    /// (Node's triggerUncaughtException from a hook or microtask); `false` when unsupported.
    #[op(name = "reportTaskError")]
    pub fn report_task_error(ctx: &mut Ctx, error: Value) -> bool {
        ctx.report_task_error(error)
    }

    /// `(hook)` — call `hook(type, promise, value)` when a settled promise is resolved again
    /// (process 'multipleResolves'); a non-function removes it.
    #[op(name = "setMultipleResolvesHook")]
    pub fn set_multiple_resolves_hook(ctx: &mut Ctx, hook: Value) {
        ctx.set_multiple_resolves_hook(hook);
    }

    /// `__node.native()`: a fresh object holding every `native` op (called once by preamble.js).
    #[op]
    pub fn native(ctx: &mut Ctx) -> Result<Value, Value> {
        ctx.module_object::<crate::native::Module>()
    }

    #[op(name = "heapObjectCount")]
    pub fn heap_object_count(ctx: &mut Ctx) -> f64 {
        ctx.live_object_count() as f64
    }

    #[op(name = "drainMicrotasks")]
    pub fn drain_microtasks(ctx: &mut Ctx) {
        ctx.drain_microtasks_for_host();
    }

    #[op(coerce, name = "transformJsx")]
    pub fn transform_jsx(source: &str, ts: Option<bool>) -> Result<String, OpError> {
        // Bun's default classic output keeps transformSync usable as a standalone script.
        let options = lumen::JsxOptions {
            runtime: lumen::JsxRuntime::Classic,
            ..Default::default()
        };
        lumen::transpile_jsx(source, ts.unwrap_or(false), &options).map_err(|error| {
            OpError::syntax_error(format!("{} (line {})", error.message, error.line))
        })
    }

    /// `stripTypes(code)`: Node's strip-only TypeScript erasure (the engine parser's TypeScript
    /// mode), offsets kept.
    /// Throws a SyntaxError with Node's `code` (ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX /
    /// ERR_INVALID_TYPESCRIPT_SYNTAX).
    #[op(coerce, name = "stripTypes")]
    pub fn strip_types(code: &str) -> Result<String, OpError> {
        lumen::typescript::strip_types(code).map_err(|e| {
            // Node's `stack` for these: the code frame (no file name), then the header.
            let stack = lumen::typescript::node_error_text(
                "",
                code,
                Some((e.offset, e.end)),
                e.line,
                &e.message,
            );
            let err = OpError::syntax_error(e.message);
            let err = match e.code {
                Some(c) => err.with_code(c),
                None => err,
            };
            err.with_prop("stack", stack)
        })
    }

    /// `compileCommonJS(source, filename, ts)`: a CommonJS module as the function
    /// `(exports, require, module, __filename, __dirname) => { source }`, compiled with no
    /// synthesized header, so positions are the file's; its frames print as `filename`. With `ts`
    /// the source is TypeScript (the engine's strip-only mode; errors carry Node's `code`).
    #[op(coerce, name = "compileCommonJS")]
    pub fn compile_commonjs(
        ctx: &mut Ctx,
        source: &str,
        filename: &str,
        ts: Value,
    ) -> Result<Value, Value> {
        let ts = matches!(ts, Value::Bool(true));
        ctx.compile_cjs_function(source, &lumen::typescript::CJS_PARAMS, filename, ts)
    }
}

/// `YYYYMMDD.HHMMSS` in local time, as Node's diagnostic file names carry it.
fn local_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    match lumen_os::time::localtime(secs) {
        Ok(tm) => format!(
            "{:04}{:02}{:02}.{:02}{:02}{:02}",
            tm.year, tm.mon, tm.mday, tm.hour, tm.min, tm.sec
        ),
        Err(_) => format!("{}.{:06}", secs / 86_400, secs % 86_400),
    }
}

/// `__os`: OS facts the JS `os` shim reads.
#[lumen_bind::module(name = "__os")]
mod os_bindings {
    use super::*;

    /// One object of OS facts the JS `os` shim reads (snapshotted like Node's are). `hostname`
    /// is separate because it can do I/O.
    #[op]
    pub fn info(ctx: &mut Ctx) -> Value {
        #[cfg(target_arch = "wasm32")]
        let (os_name, arch_name) = ("linux", "wasm32");
        #[cfg(not(target_arch = "wasm32"))]
        let (os_name, arch_name) = (std::env::consts::OS, std::env::consts::ARCH);
        let platform = match os_name {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let arch = match arch_name {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "ia32",
            other => other,
        };
        let os_type = match os_name {
            "macos" => "Darwin",
            "linux" => "Linux",
            "windows" => "Windows_NT",
            other => other,
        };
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .filter(|home| !home.is_empty())
            .or_else(|| lumen_os::ident::current_user().dir)
            .unwrap_or_default();
        let tmpdir = lumen_os::sysinfo::tmpdir();
        let cpus = lumen_os::sysinfo::cpu_count();
        let [_, _, release, version, _] = lumen_os::proc::uname().unwrap_or_default();

        let obj = Value::Obj(ctx.new_object());
        for (k, v) in [
            ("platform", platform.to_string()),
            ("arch", arch.to_string()),
            ("type", os_type.to_string()),
            ("homedir", home),
            ("tmpdir", tmpdir),
            ("release", release),
            ("version", version),
            (
                "endianness",
                if cfg!(target_endian = "big") {
                    "BE"
                } else {
                    "LE"
                }
                .to_string(),
            ),
        ] {
            let _ = ctx.set_member(&obj, k, Value::from_string(v));
        }
        let _ = ctx.set_member(&obj, "cpus", Value::Num(cpus as f64));
        let (cpu_model, cpu_speed_mhz, total_mem) = lumen_os::sysinfo::cpu_and_memory();
        let _ = ctx.set_member(&obj, "cpuModel", Value::from_string(cpu_model));
        let _ = ctx.set_member(&obj, "cpuSpeed", Value::Num(cpu_speed_mhz as f64));
        let _ = ctx.set_member(&obj, "totalmem", Value::Num(total_mem as f64));
        obj
    }

    /// Best-effort hostname without a syscall or crate: env, then the file Linux writes it to,
    /// then a safe default.
    #[op]
    pub fn hostname() -> String {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "localhost".to_string())
    }

    /// Live facts for `os.uptime/loadavg/freemem/userInfo`: `{ uptime, load1/5/15, freemem, uid, gid,
    /// username, shell, homedir }`, read fresh on each call.
    #[op]
    pub fn sysinfo(ctx: &mut Ctx) -> Value {
        let obj = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&obj, "uptime", Value::Num(lumen_os::sysinfo::uptime()));
        let _ = ctx.set_member(
            &obj,
            "freemem",
            Value::Num(lumen_os::sysinfo::free_memory()),
        );
        let load = lumen_os::sysinfo::loadavg();
        for (key, n) in ["load1", "load5", "load15"].into_iter().zip(load) {
            let _ = ctx.set_member(&obj, key, Value::Num(n));
        }
        let user = lumen_os::ident::current_user();
        let _ = ctx.set_member(&obj, "uid", Value::Num(user.uid as f64));
        let _ = ctx.set_member(&obj, "gid", Value::Num(user.gid as f64));
        let _ = ctx.set_member(
            &obj,
            "username",
            Value::from_string(user.name.unwrap_or_default()),
        );
        let shell = user.shell.filter(|s| !s.is_empty());
        let _ = ctx.set_member(
            &obj,
            "shell",
            shell.map(Value::from_string).unwrap_or(Value::Null),
        );
        let _ = ctx.set_member(
            &obj,
            "homedir",
            Value::from_string(user.dir.unwrap_or_default()),
        );
        obj
    }

    /// `{ errno, code }` for os.js to build Node's ERR_SYSTEM_ERROR from.
    fn priority_error(ctx: &mut Ctx, e: lumen_os::FsError) -> Value {
        let obj = Value::Obj(ctx.new_object());
        let errno = lumen_os::uv::errno(e.code()).unwrap_or(-e.errno());
        let _ = ctx.set_member(&obj, "errno", Value::Num(errno as f64));
        let _ = ctx.set_member(&obj, "code", Value::str(e.code()));
        obj
    }

    /// The numeric priority, or `{ errno, code }` on failure.
    #[op(coerce, name = "getPriority")]
    pub fn get_priority(ctx: &mut Ctx, pid: f64) -> Value {
        match lumen_os::sysinfo::get_priority(pid as i32) {
            Ok(p) => Value::Num(p as f64),
            Err(e) => priority_error(ctx, e),
        }
    }

    /// `undefined` on success, or `{ errno, code }` on failure.
    #[op(coerce, name = "setPriority")]
    pub fn set_priority(ctx: &mut Ctx, pid: f64, priority: f64) -> Value {
        match lumen_os::sysinfo::set_priority(pid as i32, priority as i32) {
            Ok(()) => Value::Undefined,
            Err(e) => priority_error(ctx, e),
        }
    }
}

/// Finalize native addon producers while the owning realm remains alive.
pub fn shutdown_native_addons(ctx: &mut Ctx) {
    napi::shutdown(ctx);
}

/// Close every socket and listener the realm still holds, ending the threads blocked on them.
pub fn close_native_io(ctx: &mut Ctx) {
    #[cfg(not(target_arch = "wasm32"))]
    net::close_all(ctx);
    #[cfg(target_arch = "wasm32")]
    let _ = ctx;
}

/// What dropping the realm does besides freeing memory — addon cleanup hooks, then closing open
/// SQLite databases (which checkpoints their write-ahead logs) — for an exit that skips the drop.
pub fn shutdown_native_resources(ctx: &mut Ctx) {
    napi::shutdown(ctx);
    #[cfg(all(feature = "bun", not(target_arch = "wasm32")))]
    sqlite::close_all(ctx);
}

/// Wake child realms blocked on their stdio pipes (see `child::close_child_pipes`).
pub fn close_child_pipes(ctx: &mut Ctx) {
    child::close_child_pipes(ctx);
}
