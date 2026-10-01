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

use lumen_host::{ops, Ctx, Extension, SpawnHandle, Value};

#[cfg(feature = "bun")]
mod bunhash;
#[cfg(not(target_arch = "wasm32"))]
mod child;
mod codec;
mod glue_dev;
#[cfg(windows)]
mod win_spawn;
#[cfg(windows)]
mod win_pipe;
mod crypto;
#[cfg(not(target_arch = "wasm32"))]
mod dns;
#[cfg(not(target_arch = "wasm32"))]
mod dylib;
#[cfg(all(feature = "bun", not(target_arch = "wasm32")))]
mod ffi;
use lumen_common::hash;
#[path = "../../lumen-runtime/src/jsx.rs"]
mod jsx;
#[cfg(not(target_arch = "wasm32"))]
mod napi;
#[cfg(not(target_arch = "wasm32"))]
mod fsb;
#[cfg(target_arch = "wasm32")]
#[path = "fsb_vfs.rs"]
mod fsb;
mod native;
#[cfg(not(target_arch = "wasm32"))]
mod net;
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
mod browser;
#[cfg(target_arch = "wasm32")]
use browser::{child, dns, napi, net, tls, vm_timeout};
#[cfg(all(feature = "bun", target_arch = "wasm32"))]
use browser::{ffi, sqlite};

/// The runtime's blocking-work spawner (threadpool), for async ops.
fn spawn_handle(ctx: &mut Ctx) -> SpawnHandle {
    ctx.op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone()
}

pub use signals::SigintBreak;

pub fn extension() -> Extension {
    let dev_glue = glue_dev::source();
    Extension {
        name: "node",
        modules: &[],
        globals: &[],
        namespaces: &[
            (
                "__node",
                ops![
                    "realmCwd" (0) => op_realm_cwd,
                    "native" (0) => native::op_native,
                    "cryptoBinding" (0) => crypto::op_crypto_binding,
                    "fsBinding" (0) => fsb::op_fs_binding,
                    "isFile" (1) => op_is_file,
                    "isDir" (1) => op_is_dir,
                    "readText" (1) => op_read_text,
                    "readBytes" (1) => op_read_bytes,
                    "stripShebang" (1) => op_strip_shebang,
                    "realpath" (1) => op_realpath,
                    "pathResolve" (0) => pathops::op_resolve,
                    "pathJoin" (0) => pathops::op_join,
                    "pathDirname" (1) => pathops::op_dirname,
                    "pathBasename" (2) => pathops::op_basename,
                    "pathExtname" (1) => pathops::op_extname,
                    "loadNativeAddon" (1) => napi::op_load_addon,
                    "isProxy" (1) => op_is_proxy,
                    "nameSource" (2) => op_name_source,
                    "promiseState" (1) => op_promise_state,
                    "proxyParts" (1) => op_proxy_parts,
                    "previewEntries" (2) => op_preview_entries,
                    "collectGarbage" (0) => op_collect_garbage,
                    "signalNumber" (1) => signals::op_signal_number,
                    "signalWatch" (2) => signals::op_signal_watch,
                    "signalUnwatch" (1) => signals::op_signal_unwatch,
                    "signalRaise" (1) => signals::op_signal_raise,
                    "sigintWatchdogStart" (0) => signals::op_sigint_watchdog_start,
                    "sigintWatchdogStop" (0) => signals::op_sigint_watchdog_stop,
                    "sigintWatchdogPending" (0) => signals::op_sigint_watchdog_pending,
                    "v8SetFlags" (1) => op_v8_set_flags,
                    "heapSnapshot" (1) => op_heap_snapshot,
                    "setNearHeapLimit" (3) => op_set_near_heap_limit,
                    "perfTiming" (0) => op_perf_timing,
                    "memoryStats" (0) => op_memory_stats,
                    "gcObserve" (1) => op_gc_observe,
                    "gcTake" (0) => op_gc_take,
                    "asyncContextGet" (0) => op_async_context_get,
                    "asyncContextSet" (1) => op_async_context_set,
                    "setPromiseHooks" (4) => op_set_promise_hooks,
                    "reportTaskError" (1) => op_report_task_error,
                    "setMultipleResolvesHook" (1) => op_set_multiple_resolves_hook,
                    "heapObjectCount" (0) => op_heap_object_count,
                    "drainMicrotasks" (0) => op_drain_microtasks,
                    "transformJsx" (1) => op_transform_jsx,
                    "stripTypes" (1) => op_strip_types,
                    "compileCommonJS" (3) => op_compile_commonjs,
                ],
            ),
            (
                "__os",
                ops![
                    "info" (0) => op_os_info,
                    "hostname" (0) => op_hostname,
                    "sysinfo" (0) => op_os_sysinfo,
                    "getPriority" (1) => op_os_getpriority,
                    "setPriority" (2) => op_os_setpriority,
                ],
            ),
            (
                "__zlib",
                ops![
                    "zstdCompress" (1) => zlib::op_zstd_compress,
                    "zstdDecompress" (1) => zlib::op_zstd_decompress,
                    "crc32" (2) => zlib::op_crc32,
                    "handleOpen" (6) => zlib::op_handle_open,
                    "handleWrite" (8) => zlib::op_handle_write,
                    "handleParams" (3) => zlib::op_handle_params,
                    "handleReset" (1) => zlib::op_handle_reset,
                    "handleClose" (1) => zlib::op_handle_close,
                ],
            ),
            #[cfg(feature = "bun")]
            (
                "__ffi",
                ops![
                    "dlopen" (1) => ffi::op_dlopen,
                    "dlsym" (2) => ffi::op_dlsym,
                    "dlclose" (1) => ffi::op_dlclose,
                    "call" (4) => ffi::op_call,
                    "ptr" (2) => ffi::op_ptr,
                    "read" (3) => ffi::op_read,
                    "readCString" (3) => ffi::op_read_cstring,
                    "toArrayBuffer" (3) => ffi::op_to_array_buffer,
                    "toBuffer" (3) => ffi::op_to_buffer,
                    "registerCallback" (3) => ffi::op_register_callback,
                    "unregisterCallback" (1) => ffi::op_unregister_callback,
                    "cc" (2) => ffi::op_cc,
                ],
            ),
            #[cfg(feature = "bun")]
            (
                "__bunhash",
                ops![
                    "wyhash" (2) => bunhash::op_wyhash,
                    "cityHash32" (2) => bunhash::op_city_hash32,
                    "cityHash64" (2) => bunhash::op_city_hash64,
                    "xxHash32" (2) => bunhash::op_xx_hash32,
                    "xxHash64" (2) => bunhash::op_xx_hash64,
                    "xxHash3" (2) => bunhash::op_xx_hash3,
                    "murmur32v3" (2) => bunhash::op_murmur32v3,
                    "murmur32v2" (2) => bunhash::op_murmur32v2,
                    "murmur64v2" (2) => bunhash::op_murmur64v2,
                    "rapidhash" (2) => bunhash::op_rapidhash,
                ],
            ),
            ("__child", child::CHILD_OPS),
            ("__vm", vm_timeout::VM_OPS),
            ("__vmc", vm_context::VM_CONTEXT_OPS),
            ("__net", net::NET_OPS),
            ("__udp", net::UDP_OPS),
            ("__tls", tls::TLS_OPS),
            ("__password", password::PASSWORD_OPS),
            #[cfg(feature = "bun")]
            ("__sqlite", sqlite::SQLITE_OPS),
            (
                "__dns",
                ops![
                    "lookup" (4) => dns::op_lookup,
                    "resolve" (4) => dns::op_resolve,
                    "getServers" (0) => dns::op_get_servers,
                    "getaddrinfo" (5) => dns::op_getaddrinfo,
                    "getnameinfo" (4) => dns::op_getnameinfo,
                ],
            ),
        ],
        state_init: Some(|state: &mut lumen_host::OpState| {
            state.put(child::ChildRegistry::default());
            state.put(net::NetRegistry::default());
            state.put(net::DgramRegistry::default());
            state.put(tls::TlsRegistry::default());
            state.put(zlib::ZlibHandles::default());
        }),
        js_init: dev_glue,
        js_init_snapshot: if dev_glue.is_some() { None } else { Some(JS_GLUE_AOT) },
    }
}

// Assembled by build.rs from src/js/*.js (single source of truth) and precompiled there to an
// ahead-of-time blob (AST, bytecode, compressed function text), loaded at boot.
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node_glue.aot"));

include!(concat!(env!("OUT_DIR"), "/esm_exports.rs"));

fn arg_path(ctx: &mut Ctx, args: &[Value]) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string())
}

fn op_is_file(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(target_arch = "wasm32")]
    return Ok(Value::Bool(lumen_host::vfs::is_file(&p)));
    #[cfg(not(target_arch = "wasm32"))]
    Ok(Value::Bool(Path::new(&p).is_file()))
}

/// `(fn, filename)` — name a CommonJS module wrapper's source for stack traces (see
/// `Ctx::name_function_source`).
fn op_name_source(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = match args.get(1) {
        Some(Value::Str(s)) => s.to_string(),
        _ => return Ok(Value::Bool(false)),
    };
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Bool(ctx.name_function_source(&f, &name)))
}

fn op_is_proxy(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(
        ctx.is_proxy_value(args.first().unwrap_or(&Value::Undefined)),
    ))
}

/// `(promise)` — `[status, value]` (0 pending, 1 fulfilled, 2 rejected) for util.inspect, or
/// `undefined` for a non-promise.
fn op_promise_state(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(match ctx.promise_state_for_host(&v) {
        Some((status, value)) => ctx.make_array(vec![Value::Num(status as f64), value]),
        None => Value::Undefined,
    })
}

/// `(proxy)` — `[target, handler]` for util.inspect's `showProxy`, or `undefined`.
fn op_proxy_parts(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(match ctx.proxy_parts_for_host(&v) {
        Some((target, handler)) => ctx.make_array(vec![target, handler]),
        None => Value::Undefined,
    })
}

/// `(iterator, isKeyValue)` — V8's `previewEntries`: a Map/Set iterator's remaining entries
/// without advancing it; `[entries, isKeyValue]` when `isKeyValue` is true, else `entries`.
fn op_preview_entries(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let Some((entries, key_value)) = ctx.preview_entries_for_host(&v) else {
        return Ok(Value::Undefined);
    };
    let list = ctx.make_array(entries);
    Ok(if matches!(args.get(1), Some(Value::Bool(true))) {
        ctx.make_array(vec![list, Value::Bool(key_value)])
    } else {
        list
    })
}

/// `(path?)` — a V8 heap snapshot of this realm's heap: written to `path`, or returned as a
/// `Uint8Array` of its JSON.
fn op_heap_snapshot(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    match args.first() {
        Some(Value::Str(path)) => {
            let path = path.to_string();
            let written = std::fs::File::create(&path).and_then(|f| {
                let mut out = std::io::BufWriter::with_capacity(64 * 1024, f);
                ctx.write_heap_snapshot(&mut out)
            });
            match written {
                Ok(()) => Ok(Value::Undefined),
                Err(e) => Err(ctx.make_error("Error", format!("cannot write heap snapshot '{path}': {e}"))),
            }
        }
        _ => {
            let mut out = Vec::new();
            ctx.write_heap_snapshot(&mut out)
                .map_err(|e| ctx.make_error("Error", format!("heap snapshot: {e}")))?;
            ctx.make_uint8array(&out)
        }
    }
}

/// `(times, threadId, dir?)` — write a heap snapshot the first `times` times the realm reaches its
/// memory ceiling (Node's `v8.setHeapSnapshotNearHeapLimit`); `0` stops. The realm then runs on
/// with a slightly higher ceiling, and the last crossing is the limit for good.
fn op_set_near_heap_limit(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let times = args.first().and_then(Value::as_num_opt).unwrap_or(0.0).max(0.0) as u32;
    let thread_id = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let dir = match args.get(2) {
        Some(Value::Str(d)) if !d.to_string().is_empty() => Some(std::path::PathBuf::from(d.to_string())),
        _ => None,
    };
    ctx.set_near_limit_hook(
        times,
        Box::new(move |interp| {
            static SEQUENCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let name = format!(
                "Heap.{}.{}.{thread_id}.{seq:03}.heapsnapshot",
                local_stamp(),
                std::process::id()
            );
            let path = dir.as_ref().map_or_else(|| std::path::PathBuf::from(&name), |d| d.join(&name));
            let _ = std::fs::File::create(&path).and_then(|f| {
                let mut out = std::io::BufWriter::with_capacity(64 * 1024, f);
                interp.write_heap_snapshot(&mut out)
            });
        }),
    );
    Ok(Value::Undefined)
}

/// `YYYYMMDD.HHMMSS` in local time, as Node's diagnostic file names carry it.
fn local_stamp() -> String {
    #[cfg(unix)]
    // SAFETY: localtime_r fills the zeroed `tm` it is given from a valid `time_t`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&now, &mut tm).is_null() {
            return format!(
                "{:04}{:02}{:02}.{:02}{:02}{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            );
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{}.{:06}", secs / 86_400, secs % 86_400)
}

/// `(flags)` — apply the V8 flags lumen implements from a `v8.setFlagsFromString` string
/// (`--allow-natives-syntax` and its `--no` form); the rest have no lumen counterpart.
fn op_v8_set_flags(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(Value::Str(flags)) = args.first() else {
        return Ok(Value::Undefined);
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
    Ok(Value::Undefined)
}

/// `()` — the current async context value (see `Interp::async_context`).
/// `(init, before, after, settled)` — install V8-style promise hooks (`v8.promiseHooks`,
/// async_hooks); all four non-callable removes them.
fn op_set_promise_hooks(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undefined);
    ctx.set_promise_hooks(Some([arg(0), arg(1), arg(2), arg(3)]));
    Ok(Value::Undefined)
}

/// `(error)` — report `error` as an uncaught exception after the current microtask checkpoint
/// (Node's triggerUncaughtException from a hook or microtask); `false` when unsupported.
fn op_report_task_error(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let error = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Bool(ctx.report_task_error(error)))
}

/// `(hook)` — call `hook(type, promise, value)` when a settled promise is resolved again
/// (process 'multipleResolves'); a non-function removes it.
fn op_set_multiple_resolves_hook(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    ctx.set_multiple_resolves_hook(args.first().cloned().unwrap_or(Value::Undefined));
    Ok(Value::Undefined)
}

fn op_async_context_get(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(ctx.async_context())
}

/// `(context)` — make `context` current, returning the previous one so the caller can restore it.
fn op_async_context_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(ctx.set_async_context(args.first().cloned().unwrap_or(Value::Undefined)))
}

fn op_collect_garbage(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(ctx.collect_garbage_for_host() as f64))
}

/// `[nodeStart, v8Start, environment, bootstrapComplete, loopStart, loopExit, idleTime]` in
/// milliseconds on the `performance.now()` clock (-1: not reached yet).
fn op_perf_timing(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let values = lumen_host::perf::snapshot().into_iter().map(Value::Num).collect();
    Ok(ctx.make_array(values))
}

/// `[liveHeapObjects, arrayBufferBytes]` for `process.memoryUsage()`.
fn op_memory_stats(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let objects = Value::Num(ctx.live_object_count() as f64);
    let buffers = Value::Num(lumen_common::buffer::tracked_bytes() as f64);
    Ok(ctx.make_array(vec![objects, buffers]))
}

/// `(callback | null)` — log every collection and queue `callback` as a microtask after it;
/// `null` stops.
fn op_gc_observe(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    match args.first() {
        Some(callback) if callback.is_callable() => {
            let mut observer = lumen::gc_log::GcObserver::new(callback.clone());
            observer.epoch_ms = lumen_host::perf::now_ms();
            ctx.op_state().put(observer);
        }
        _ => {
            ctx.op_state().take::<lumen::gc_log::GcObserver>();
        }
    }
    Ok(Value::Undefined)
}

/// `()` — the logged collections as a flat `[startTime, duration, forced, ...]` array (ms, on
/// the `performance.now()` clock), clearing the log.
fn op_gc_take(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
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
    Ok(ctx.make_array(flat))
}

fn op_heap_object_count(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(ctx.live_object_count() as f64))
}

fn op_drain_microtasks(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    ctx.drain_microtasks_for_host();
    Ok(Value::Undefined)
}

fn op_transform_jsx(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let source = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    jsx::transform(&source)
        .map(Value::from_string)
        .map_err(|message| ctx.make_error("SyntaxError", message))
}

/// `stripTypes(code)`: Node's strip-only TypeScript erasure (the engine parser's TypeScript
/// mode), offsets kept.
/// Throws a SyntaxError with Node's `code` (ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX /
/// ERR_INVALID_TYPESCRIPT_SYNTAX).
fn op_strip_types(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let source = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    match lumen::typescript::strip_types(&source) {
        Ok(s) => Ok(Value::from_string(s)),
        Err(e) => {
            // Node's `stack` for these: the code frame (no file name), then the header.
            let stack = lumen::typescript::node_error_text(
                "",
                &source,
                Some((e.offset, e.end)),
                e.line,
                &e.message,
            );
            let err = ctx.make_error("SyntaxError", e.message);
            if let Some(code) = e.code {
                let _ = ctx.set_member(&err, "code", Value::str(code));
            }
            let _ = ctx.set_member(&err, "stack", Value::from_string(stack));
            Err(err)
        }
    }
}

/// `compileCommonJS(source, filename, ts)`: a CommonJS module as the function
/// `(exports, require, module, __filename, __dirname) => { source }`, compiled with no
/// synthesized header, so positions are the file's; its frames print as `filename`. With `ts`
/// the source is TypeScript (the engine's strip-only mode; errors carry Node's `code`).
fn op_compile_commonjs(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let source = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let filename = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let ts = matches!(args.get(2), Some(Value::Bool(true)));
    ctx.compile_cjs_function(&source, &lumen::typescript::CJS_PARAMS, &filename, ts)
}

fn op_is_dir(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(target_arch = "wasm32")]
    return Ok(Value::Bool(lumen_host::vfs::is_dir(&p)));
    #[cfg(not(target_arch = "wasm32"))]
    Ok(Value::Bool(Path::new(&p).is_dir()))
}

/// Read a module/JSON source as text; a miss is an error the resolver turns into
/// MODULE_NOT_FOUND context.
fn op_read_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(target_arch = "wasm32")]
    let read = lumen_host::vfs::read_file(&p)
        .map_err(|e| e.to_io())
        .and_then(|b| String::from_utf8(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)));
    #[cfg(not(target_arch = "wasm32"))]
    let read = std::fs::read_to_string(&p);
    match read {
        Ok(s) => Ok(Value::from_string(crate::codec::canonical(s))),
        Err(e) => Err(ctx.make_error("Error", format!("cannot read '{p}': {e}"))),
    }
}

/// `(source)` — the source with a leading `#!` line marker turned into `//`, keeping every
/// offset. Done natively: any JS string method on a large source materializes a UTF-16 copy.
fn op_strip_shebang(_ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let source = args.first().cloned().unwrap_or(Value::Undefined);
    if let Value::Str(s) = &source {
        let text = s.as_str();
        let body = text.strip_prefix('\u{feff}').unwrap_or(text);
        if let Some(rest) = body.strip_prefix("#!") {
            return Ok(Value::from_string(format!("//{rest}")));
        }
        if body.len() != text.len() {
            return Ok(Value::from_string(body.to_string()));
        }
    }
    Ok(source)
}

/// Read a file as raw bytes (a Uint8Array), for `fs.readFileSync` without an encoding — the text
/// path corrupts binary. Errors carry the errno `code` Node users switch on.
fn op_read_bytes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(target_arch = "wasm32")]
    let read = lumen_host::vfs::read_file(&p).map_err(|e| e.to_io());
    #[cfg(not(target_arch = "wasm32"))]
    let read = std::fs::read(&p);
    match read {
        Ok(bytes) => ctx.make_uint8array(&bytes),
        Err(e) => {
            let err = ctx.make_error("Error", format!("cannot read '{p}': {e}"));
            let code = match e.kind() {
                std::io::ErrorKind::NotFound => "ENOENT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                _ => "EIO",
            };
            let _ = ctx.set_member(&err, "code", Value::str(code));
            Err(err)
        }
    }
}

/// Canonicalize (resolve symlinks) for the module cache key; falls back to the input when the
/// path doesn't exist yet (matching how the JS resolver probes candidates).
fn op_realpath(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(target_arch = "wasm32")]
    let canon = lumen_host::vfs::realpath(&p).map(std::path::PathBuf::from).map_err(|e| e.to_io());
    #[cfg(not(target_arch = "wasm32"))]
    let canon = lumen_host::canonicalize(&p);
    match canon {
        Ok(c) => Ok(Value::from_string(c.to_string_lossy().into_owned())),
        Err(_) => Ok(Value::from_string(p)),
    }
}

/// One object of OS facts the JS `os` shim reads (snapshotted like Node's are). `hostname`
/// is separate because it can do I/O.
fn op_os_info(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
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
        .or_else(|| lumen_os::proc::current_user().dir)
        .unwrap_or_default();
    #[cfg(target_arch = "wasm32")]
    let (tmpdir, cpus) = ("/tmp".to_string(), 1usize);
    #[cfg(not(target_arch = "wasm32"))]
    let (tmpdir, cpus) = (
        std::env::temp_dir().to_string_lossy().into_owned(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    );
    let (release, version) = os_release_version();

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
    let (cpu_model, cpu_speed_mhz, total_mem) = cpu_and_memory_facts();
    let _ = ctx.set_member(&obj, "cpuModel", Value::from_string(cpu_model));
    let _ = ctx.set_member(&obj, "cpuSpeed", Value::Num(cpu_speed_mhz as f64));
    let _ = ctx.set_member(&obj, "totalmem", Value::Num(total_mem as f64));
    Ok(obj)
}

/// `(cpu model, cpu speed in MHz, total memory in bytes)` for `os.cpus()` / `os.totalmem()`.
/// Playwright reads the model to tell Apple silicon from Intel when it picks a browser build.
#[cfg(target_os = "macos")]
fn cpu_and_memory_facts() -> (String, u64, u64) {
    extern "C" {
        fn sysctlbyname(
            name: *const std::os::raw::c_char,
            oldp: *mut std::os::raw::c_void,
            oldlenp: *mut usize,
            newp: *mut std::os::raw::c_void,
            newlen: usize,
        ) -> std::os::raw::c_int;
    }
    fn read(name: &str, buf: &mut [u8]) -> Option<usize> {
        let cname = std::ffi::CString::new(name).ok()?;
        let mut len = buf.len();
        // SAFETY: `buf` is writable for `len` bytes and `len` is updated to the bytes written.
        let rc = unsafe {
            sysctlbyname(
                cname.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(len)
    }
    let mut model = [0u8; 256];
    let model = read("machdep.cpu.brand_string", &mut model)
        .map(|n| String::from_utf8_lossy(&model[..n]).trim_end_matches('\0').to_string())
        .unwrap_or_default();
    let mut u64_buf = [0u8; 8];
    let hz = read("hw.cpufrequency", &mut u64_buf)
        .filter(|&n| n == 8)
        .map(|_| u64::from_ne_bytes(u64_buf))
        .unwrap_or(0);
    let memsize = read("hw.memsize", &mut u64_buf)
        .filter(|&n| n == 8)
        .map(|_| u64::from_ne_bytes(u64_buf))
        .unwrap_or(0);
    (model, hz / 1_000_000, memsize)
}

#[cfg(target_os = "linux")]
fn cpu_and_memory_facts() -> (String, u64, u64) {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| {
        cpuinfo
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    };
    let model = field("model name").unwrap_or_default();
    let mhz = field("cpu MHz")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0) as u64;
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0);
    (model, mhz, total)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cpu_and_memory_facts() -> (String, u64, u64) {
    (String::new(), 0, 0)
}

/// `(os.release(), os.version())` — utsname's `release` and `version` fields. std exposes no
/// uname(), so on macOS and Linux this calls it directly; Playwright reads the release to pick a
/// browser build for the running macOS, and an empty string sends it to the wrong one.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn os_release_version() -> (String, String) {
    // struct utsname is N fixed-size char arrays: 256 each on macOS, 65 each on Linux (glibc
    // adds a sixth, `domainname`). Only sysname/nodename/release/version/machine are read.
    #[cfg(target_os = "macos")]
    const FIELD: usize = 256;
    #[cfg(target_os = "linux")]
    const FIELD: usize = 65;
    #[repr(C)]
    struct Utsname([[std::os::raw::c_char; FIELD]; 6]);
    extern "C" {
        fn uname(buf: *mut Utsname) -> std::os::raw::c_int;
    }
    let mut buf = Utsname([[0; FIELD]; 6]);
    // SAFETY: `buf` is a writable utsname at least as large as the platform's struct.
    if unsafe { uname(&mut buf) } != 0 {
        return (String::new(), String::new());
    }
    let field = |i: usize| {
        let bytes: Vec<u8> = buf.0[i]
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (field(2), field(3))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_release_version() -> (String, String) {
    // No uname; Linux-style fallback for other unixes, blank elsewhere rather than an invented
    // version.
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    (release, String::new())
}

/// Best-effort hostname without a syscall or crate: env, then the file Linux writes it to,
/// then a safe default.
fn op_hostname(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let name = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string());
    Ok(Value::from_string(name))
}

// ---- os.getPriority / os.setPriority, over getpriority(2)/setpriority(2) ----
// std exposes no nice-value API, so reach libc directly (same category as the utimes/statvfs FFI
// above). `PRIO_PROCESS` is 0 on macOS and Linux. Each op returns either the numeric priority /
// undefined on success, or `{ errno, code }` on failure so os.js can build Node's ERR_SYSTEM_ERROR.
#[cfg(any(target_os = "macos", target_os = "linux"))]
extern "C" {
    fn getpriority(which: std::os::raw::c_int, who: std::os::raw::c_uint) -> std::os::raw::c_int;
    fn setpriority(
        which: std::os::raw::c_int,
        who: std::os::raw::c_uint,
        prio: std::os::raw::c_int,
    ) -> std::os::raw::c_int;
}

// The thread-local errno cell, so getpriority's -1 return can be disambiguated from a real error
// (a nice value of -1 is legal). macOS spells the accessor `__error`, Linux `__errno_location`.
#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "__error"]
    fn errno_location() -> *mut std::os::raw::c_int;
}
#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "__errno_location"]
    fn errno_location() -> *mut std::os::raw::c_int;
}

/// Map a raw errno to the code string Node reports (the subset getpriority/setpriority raise).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn priority_errno_code(errno: i32) -> &'static str {
    match errno {
        1 => "EPERM",
        3 => "ESRCH",
        13 => "EACCES",
        22 => "EINVAL",
        _ => "UNKNOWN",
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn priority_error(ctx: &mut Ctx, errno: i32) -> Value {
    let obj = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&obj, "errno", Value::Num(-(errno as f64)));
    let _ = ctx.set_member(&obj, "code", Value::str(priority_errno_code(errno)));
    obj
}

fn op_os_getpriority(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let pid = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as i64;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        // getpriority can legitimately return -1..-20, so zero errno first and consult it after.
        // SAFETY: errno_location returns a valid pointer to the thread's errno cell; PRIO_PROCESS(0)
        // with a pid touches no memory.
        let rc = unsafe {
            *errno_location() = 0;
            getpriority(0, pid as std::os::raw::c_uint)
        };
        let errno = unsafe { *errno_location() };
        if rc == -1 && errno != 0 {
            return Ok(priority_error(ctx, errno));
        }
        Ok(Value::Num(rc as f64))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        Ok(Value::Num(0.0))
    }
}

fn op_os_setpriority(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let pid = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as i64;
    let prio = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as i32;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        // SAFETY: PRIO_PROCESS(0) with a pid and an integer priority; no memory is touched.
        let rc = unsafe { setpriority(0, pid as std::os::raw::c_uint, prio) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Ok(priority_error(ctx, errno));
        }
        Ok(Value::Undefined)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pid, prio);
        Ok(Value::Undefined)
    }
}

/// `() -> string | undefined` — the embedded realm's working directory, or `undefined` when the
/// runtime owns its process (the OS cwd is then the right base and paths stay as written).
fn op_realm_cwd(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(ctx
        .op_state()
        .get::<lumen_host::RealmProcess>()
        .map(|realm| Value::from_string(realm.cwd.to_string_lossy().into_owned()))
        .unwrap_or(Value::Undefined))
}

/// Finalize native addon producers while the owning realm remains alive.
pub fn shutdown_native_addons(ctx: &mut Ctx) { napi::shutdown(ctx); }

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
pub fn close_child_pipes(ctx: &mut Ctx) { child::close_child_pipes(ctx); }

/// Live facts for `os.uptime/loadavg/freemem/userInfo`: `{ uptime, load1/5/15, freemem, uid, gid,
/// username, shell, homedir }`, read fresh on each call.
fn op_os_sysinfo(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let obj = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&obj, "uptime", Value::Num(lumen_os::proc::uptime()));
    let _ = ctx.set_member(&obj, "freemem", Value::Num(lumen_os::proc::free_memory()));
    let load = lumen_os::proc::loadavg();
    for (key, n) in ["load1", "load5", "load15"].into_iter().zip(load) {
        let _ = ctx.set_member(&obj, key, Value::Num(n));
    }
    let user = lumen_os::proc::current_user();
    let _ = ctx.set_member(&obj, "uid", Value::Num(user.uid as f64));
    let _ = ctx.set_member(&obj, "gid", Value::Num(user.gid as f64));
    let _ = ctx.set_member(&obj, "username", Value::from_string(user.name.unwrap_or_default()));
    let shell = user.shell.filter(|s| !s.is_empty());
    let _ = ctx.set_member(&obj, "shell", shell.map(Value::from_string).unwrap_or(Value::Null));
    let _ = ctx.set_member(&obj, "homedir", Value::from_string(user.dir.unwrap_or_default()));
    Ok(obj)
}

