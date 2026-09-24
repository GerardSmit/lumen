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
//! - [x] `package.json` `main`; a practical `exports` subset (string, `"."` key,
//!   `require`/`node`/`default` conditions) — [ ] subpath patterns, full conditional exports
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
mod child;
mod codec;
#[cfg(windows)]
mod win_spawn;
#[cfg(windows)]
mod win_pipe;
mod crypto;
mod dns;
mod dylib;
#[cfg(feature = "bun")]
mod ffi;
mod hash;
#[path = "../../lumen-runtime/src/jsx.rs"]
mod jsx;
mod napi;
mod fsb;
mod native;
mod net;
mod password;
#[cfg(feature = "bun")]
mod sqlite;
mod tls;
mod vm_timeout;

/// The runtime's blocking-work spawner (threadpool), for async ops.
fn spawn_handle(ctx: &mut Ctx) -> SpawnHandle {
    ctx.op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone()
}

pub fn extension() -> Extension {
    Extension {
        name: "node",
        globals: &[],
        namespaces: &[
            (
                "__node",
                ops![
                    "realmCwd" (0) => op_realm_cwd,
                    "native" (0) => native::op_native,
                    "fsBinding" (0) => fsb::op_fs_binding,
                    "isFile" (1) => op_is_file,
                    "isDir" (1) => op_is_dir,
                    "readText" (1) => op_read_text,
                    "readBytes" (1) => op_read_bytes,
                    "realpath" (1) => op_realpath,
                    "loadNativeAddon" (1) => napi::op_load_addon,
                    "isProxy" (1) => op_is_proxy,
                    "nameSource" (2) => op_name_source,
                    "promiseState" (1) => op_promise_state,
                    "proxyParts" (1) => op_proxy_parts,
                    "collectGarbage" (0) => op_collect_garbage,
                    "asyncContextGet" (0) => op_async_context_get,
                    "asyncContextSet" (1) => op_async_context_set,
                    "heapObjectCount" (0) => op_heap_object_count,
                    "drainMicrotasks" (0) => op_drain_microtasks,
                    "transformJsx" (1) => op_transform_jsx,
                    "stripTypes" (1) => op_strip_types,
                    "compileCommonJS" (3) => op_compile_commonjs,
                    "stat" (1) => op_stat,
                    "lstat" (1) => op_lstat,
                    "rm" (3) => op_rm,
                    "mkdir" (2) => op_mkdir,
                    "rename" (2) => op_rename,
                    "copyFile" (2) => op_copy_file,
                    "readdirTypes" (1) => op_readdir_types,
                    "readlink" (1) => op_readlink,
                    "symlink" (2) => op_symlink,
                    "access" (1) => op_access,
                    "chmod" (2) => op_chmod,
                    "chown" (3) => op_chown,
                    "lchown" (3) => op_lchown,
                    "lchmod" (2) => op_lchmod,
                    "mkdtemp" (1) => op_mkdtemp,
                    "link" (2) => op_link,
                    "utimes" (3) => op_utimes,
                    "lutimes" (3) => op_lutimes,
                    "statfs" (1) => op_statfs,
                ],
            ),
            (
                "__os",
                ops![
                    "info" (0) => op_os_info,
                    "hostname" (0) => op_hostname,
                    "getPriority" (1) => op_os_getpriority,
                    "setPriority" (2) => op_os_setpriority,
                ],
            ),
            (
                "__zlib",
                ops![
                    "deflate" (1) => op_zlib_deflate,
                    "inflate" (1) => op_zlib_inflate,
                    "deflateRaw" (1) => op_zlib_deflate_raw,
                    "inflateRaw" (1) => op_zlib_inflate_raw,
                    "gzip" (1) => op_zlib_gzip,
                    "gunzip" (1) => op_zlib_gunzip,
                    "brotliCompress" (1) => op_zlib_brotli_compress,
                    "brotliDecompress" (1) => op_zlib_brotli_decompress,
                    "zstdCompress" (1) => op_zlib_zstd_compress,
                    "zstdDecompress" (1) => op_zlib_zstd_decompress,
                    "crc32" (2) => op_zlib_crc32,
                    "streamOpen" (1) => op_zlib_stream_open,
                    "streamWrite" (2) => op_zlib_stream_write,
                    "streamFlush" (2) => op_zlib_stream_flush,
                    "streamReset" (1) => op_zlib_stream_reset,
                    "streamClose" (1) => op_zlib_stream_close,
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
            ("__net", net::NET_OPS),
            ("__udp", net::UDP_OPS),
            ("__tls", tls::TLS_OPS),
            ("__crypto", crypto::CRYPTO_OPS),
            ("__password", password::PASSWORD_OPS),
            #[cfg(feature = "bun")]
            ("__sqlite", sqlite::SQLITE_OPS),
            (
                "__dns",
                ops![
                    "lookup" (4) => dns::op_lookup,
                    "resolve" (4) => dns::op_resolve,
                    "getServers" (0) => dns::op_get_servers,
                ],
            ),
        ],
        state_init: Some(|state: &mut lumen_host::OpState| {
            state.put(child::ChildRegistry::default());
            state.put(net::NetRegistry::default());
            state.put(net::DgramRegistry::default());
            state.put(tls::TlsRegistry::default());
            state.put(ZlibStreams::default());
        }),
        js_init: None,
        js_init_snapshot: Some(JS_GLUE_AOT),
    }
}

// Assembled by build.rs from src/js/*.js (single source of truth) and precompiled there to an
// ahead-of-time blob (AST, bytecode, compressed function text), loaded at boot.
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node_glue.aot"));

fn arg_path(ctx: &mut Ctx, args: &[Value]) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string())
}

fn op_is_file(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
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

/// `()` — the current async context value (see `Interp::async_context`).
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
    Ok(Value::Bool(Path::new(&p).is_dir()))
}

/// Read a module/JSON source as text; a miss is an error the resolver turns into
/// MODULE_NOT_FOUND context.
fn op_read_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(Value::from_string(crate::codec::canonical(s))),
        Err(e) => Err(ctx.make_error("Error", format!("cannot read '{p}': {e}"))),
    }
}

/// Read a file as raw bytes (a Uint8Array), for `fs.readFileSync` without an encoding — the text
/// path corrupts binary. Errors carry the errno `code` Node users switch on.
fn op_read_bytes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read(&p) {
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
    match lumen_host::canonicalize(&p) {
        Ok(c) => Ok(Value::from_string(c.to_string_lossy().into_owned())),
        Err(_) => Ok(Value::from_string(p)),
    }
}

/// Build a Stats-shaped object from real filesystem metadata (the JS glue wraps it with the
/// `isFile()`/`isDirectory()`/… methods). `kind` lets the glue answer those without re-probing.
fn stat_object(ctx: &mut Ctx, meta: &std::fs::Metadata) -> Value {
    let ms = |t: std::io::Result<std::time::SystemTime>| -> f64 {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    };
    let ft = meta.file_type();
    let kind = if ft.is_file() {
        "file"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let o = Value::Obj(ctx.new_object());
    let set = |o: &Value, k: &str, v: f64, ctx: &mut Ctx| {
        let _ = ctx.set_member(o, k, Value::Num(v));
    };
    set(&o, "size", meta.len() as f64, ctx);
    set(&o, "mtimeMs", ms(meta.modified()), ctx);
    set(&o, "atimeMs", ms(meta.accessed()), ctx);
    set(&o, "birthtimeMs", ms(meta.created()), ctx);
    // The rest of the Node `Stats` fields are OS-specific. Unix reads them straight from the inode;
    // Windows has no inode/uid/gid/mode, so — as Node does — we emulate: `mode` from the read-only
    // attribute + file type, `ino`/`dev` from the file index + volume serial, and 0/defaults for
    // the POSIX-only fields.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        set(&o, "mode", meta.mode() as f64, ctx);
        // Unix has no true creation time everywhere; `ctime` is the inode change time.
        set(
            &o,
            "ctimeMs",
            meta.ctime() as f64 * 1000.0 + meta.ctime_nsec() as f64 / 1e6,
            ctx,
        );
        set(&o, "ino", meta.ino() as f64, ctx);
        set(&o, "dev", meta.dev() as f64, ctx);
        set(&o, "nlink", meta.nlink() as f64, ctx);
        set(&o, "uid", meta.uid() as f64, ctx);
        set(&o, "gid", meta.gid() as f64, ctx);
        set(&o, "rdev", meta.rdev() as f64, ctx);
        set(&o, "blksize", meta.blksize() as f64, ctx);
        set(&o, "blocks", meta.blocks() as f64, ctx);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
        let readonly = meta.file_attributes() & FILE_ATTRIBUTE_READONLY != 0;
        // POSIX-style mode, matching Node's Windows emulation: type bits | rwx (0o666/0o444 for
        // files, 0o777/0o555 for directories) with the write bits cleared when read-only.
        let type_bits = if ft.is_dir() {
            0o040000
        } else if ft.is_symlink() {
            0o120000
        } else {
            0o100000
        };
        let perm = match (ft.is_dir(), readonly) {
            (true, false) => 0o777,
            (true, true) => 0o555,
            (false, false) => 0o666,
            (false, true) => 0o444,
        };
        set(&o, "mode", (type_bits | perm) as f64, ctx);
        // Windows has no change time; Node reports the last-write time here.
        set(&o, "ctimeMs", ms(meta.modified()), ctx);
        // `ino`/`dev`/`nlink` come from the by-handle info that stable std doesn't expose (the
        // `windows_by_handle` feature is nightly-only), and lumen takes no `winapi`-style
        // dependency, so report the neutral defaults rather than call into the Win32 API.
        set(&o, "ino", 0.0, ctx);
        set(&o, "dev", 0.0, ctx);
        set(&o, "nlink", 1.0, ctx);
        set(&o, "uid", 0.0, ctx);
        set(&o, "gid", 0.0, ctx);
        set(&o, "rdev", 0.0, ctx);
        set(&o, "blksize", 4096.0, ctx);
        set(&o, "blocks", ((meta.len() + 511) / 512) as f64, ctx);
    }
    let _ = ctx.set_member(&o, "kind", Value::str(kind));
    o
}

/// An errno-tagged filesystem error (the `code` Node users switch on), from a `std::io::Error`.
fn fs_error(ctx: &mut Ctx, op: &str, path: &str, e: &std::io::Error) -> Value {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        _ => match e.raw_os_error() {
            Some(20) => "ENOTDIR",
            Some(21) => "EISDIR",
            Some(39) | Some(66) => "ENOTEMPTY",
            _ => "EIO",
        },
    };
    let err = ctx.make_error("Error", format!("{code}: {e}, {op} '{path}'"));
    let _ = ctx.set_member(&err, "code", Value::str(code));
    let _ = ctx.set_member(&err, "path", Value::str(path));
    let _ = ctx.set_member(&err, "syscall", Value::str(op));
    err
}

/// `stat(path)` — follow symlinks.
fn op_stat(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::metadata(&p) {
        Ok(m) => Ok(stat_object(ctx, &m)),
        Err(e) => Err(fs_error(ctx, "stat", &p, &e)),
    }
}

/// `lstat(path)` — do not follow symlinks.
fn op_lstat(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::symlink_metadata(&p) {
        Ok(m) => Ok(stat_object(ctx, &m)),
        Err(e) => Err(fs_error(ctx, "lstat", &p, &e)),
    }
}

/// `rm(path, recursive, force)` — remove a file or directory tree.
fn op_rm(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let recursive = matches!(args.get(1), Some(Value::Bool(true)));
    let force = matches!(args.get(2), Some(Value::Bool(true)));
    let path = std::path::Path::new(&p);
    let result = if path.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(Value::Undefined),
        // `force` swallows a missing path, as Node's rmSync does.
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "rm", &p, &e)),
    }
}

/// `mkdir(path, recursive)` — returns the first created directory (Node's recursive contract).
fn op_mkdir(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let recursive = matches!(args.get(1), Some(Value::Bool(true)));
    let result = if recursive {
        std::fs::create_dir_all(&p)
    } else {
        std::fs::create_dir(&p)
    };
    match result {
        Ok(()) => Ok(Value::from_string(p)),
        Err(e) if recursive && e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(Value::Undefined)
        }
        Err(e) => Err(fs_error(ctx, "mkdir", &p, &e)),
    }
}

/// `rename(from, to)`.
fn op_rename(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let from = arg_path(ctx, args)?;
    let to = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    match std::fs::rename(&from, &to) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "rename", &from, &e)),
    }
}

/// `copyFile(from, to)`.
fn op_copy_file(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let from = arg_path(ctx, args)?;
    let to = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    match std::fs::copy(&from, &to) {
        Ok(_) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "copyfile", &from, &e)),
    }
}

/// `readdirTypes(path)` — directory entries as `[name, kind]` pairs for `withFileTypes`.
fn op_readdir_types(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let entries = std::fs::read_dir(&p).map_err(|e| fs_error(ctx, "scandir", &p, &e))?;
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match entry.file_type() {
            Ok(ft) if ft.is_dir() => "dir",
            Ok(ft) if ft.is_symlink() => "symlink",
            Ok(ft) if ft.is_file() => "file",
            _ => "other",
        };
        let pair = ctx.make_array(vec![Value::from_string(name), Value::str(kind)]);
        items.push(pair);
    }
    Ok(ctx.make_array(items))
}

/// `readlink(path)`.
fn op_readlink(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read_link(&p) {
        Ok(target) => Ok(Value::from_string(target.to_string_lossy().into_owned())),
        Err(e) => Err(fs_error(ctx, "readlink", &p, &e)),
    }
}

/// `symlink(target, path)`.
fn op_symlink(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = arg_path(ctx, args)?;
    let link = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    // Unix has one symlink call; Windows distinguishes file vs directory links (and needs the
    // privilege / developer mode to create them), so pick by what the target currently is — the
    // same heuristic Node uses when its `type` argument is omitted.
    #[cfg(unix)]
    let r = std::os::unix::fs::symlink(&target, &link);
    #[cfg(windows)]
    let r = if std::fs::metadata(&target)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        std::os::windows::fs::symlink_dir(&target, &link)
    } else {
        std::os::windows::fs::symlink_file(&target, &link)
    };
    match r {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "symlink", &link, &e)),
    }
}

/// `access(path)` — existence/readability check; rejects with ENOENT if absent.
fn op_access(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::metadata(&p) {
        Ok(_) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "access", &p, &e)),
    }
}

/// `chmod(path, mode)`.
fn op_chmod(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let mode = args.get(1).and_then(|v| v.as_num_opt()).unwrap_or(0.0) as u32;
    // Unix applies the full POSIX mode. Windows has no POSIX permissions — the only bit it can
    // honour (as Node does) is read-only: clear it when the owner-write bit is set, set it
    // otherwise.
    #[cfg(unix)]
    let perms = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    };
    #[cfg(windows)]
    let perms = match std::fs::metadata(&p) {
        Ok(m) => {
            let mut perms = m.permissions();
            perms.set_readonly(mode & 0o200 == 0);
            perms
        }
        Err(e) => return Err(fs_error(ctx, "chmod", &p, &e)),
    };
    match std::fs::set_permissions(&p, perms) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "chmod", &p, &e)),
    }
}

#[cfg(unix)]
fn ownership_path(ctx: &mut Ctx, args: &[Value], op: &str, follow: bool) -> Result<Value, Value> {
    let path = arg_path(ctx, args)?;
    let uid = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let gid = args.get(2).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let result = if follow {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
    } else {
        std::os::unix::fs::lchown(&path, Some(uid), Some(gid))
    };
    result
        .map(|()| Value::Undefined)
        .map_err(|error| fs_error(ctx, op, &path, &error))
}

fn op_chown(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    #[cfg(unix)]
    {
        ownership_path(ctx, args, "chown", true)
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err(ctx.make_error("Error", "ENOSYS: chown is not supported on this platform"))
    }
}

fn op_lchown(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    #[cfg(unix)]
    {
        ownership_path(ctx, args, "lchown", false)
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err(ctx.make_error("Error", "ENOSYS: lchown is not supported on this platform"))
    }
}

fn op_lchmod(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_path(ctx, args)?;
    let mode = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(std::ffi::OsStr::new(&path).as_bytes())
            .map_err(|_| ctx.make_error("TypeError", "path must not contain NUL bytes"))?;
        if unsafe { lchmod(c_path.as_ptr(), mode) } == 0 {
            Ok(Value::Undefined)
        } else {
            Err(fs_error(
                ctx,
                "lchmod",
                &path,
                &std::io::Error::last_os_error(),
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = mode;
        Err(ctx.make_error(
            "Error",
            format!("ENOSYS: lchmod is not supported on this platform, lchmod '{path}'"),
        ))
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn lchmod(path: *const std::os::raw::c_char, mode: u32) -> i32;
}

/// `mkdtemp(prefix)` — create a uniquely-named temp directory and return its path. Uniqueness
/// comes from the OS-assigned pid plus a monotonically bumped counter (no RNG dependency).
fn op_mkdtemp(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let prefix = arg_path(ctx, args)?;
    let pid = std::process::id();
    for _ in 0..1000 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{prefix}{:06x}{:04x}", pid, n & 0xffff);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(Value::from_string(candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fs_error(ctx, "mkdtemp", &candidate, &e)),
        }
    }
    Err(ctx.make_error("Error", format!("mkdtemp '{prefix}': exhausted candidates")))
}

/// `link(existingPath, newPath)` — create a hard link.
fn op_link(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let existing = arg_path(ctx, args)?;
    let new = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    match std::fs::hard_link(&existing, &new) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "link", &existing, &e)),
    }
}

/// `utimes(path, atimeSec, mtimeSec)` / `lutimes(...)` — set access/modification times.
/// Backed by the BSD `utimes(2)`/`lutimes(2)` (present on macOS and glibc Linux); a clean
/// ENOSYS error elsewhere. `lutimes` acts on the link itself rather than its target.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    #[cfg(target_os = "macos")]
    tv_usec: i32,
    #[cfg(target_os = "linux")]
    tv_usec: i64,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn timeval_from_secs(secs: f64) -> Timeval {
    let s = secs.floor();
    let usec = ((secs - s) * 1_000_000.0).round();
    Timeval {
        tv_sec: s as i64,
        tv_usec: usec as _,
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
extern "C" {
    fn utimes(path: *const std::os::raw::c_char, times: *const Timeval) -> i32;
    fn lutimes(path: *const std::os::raw::c_char, times: *const Timeval) -> i32;
}

fn set_times(ctx: &mut Ctx, args: &[Value], follow: bool, op: &str) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let atime = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    let mtime = ctx.coerce_number(args.get(2).unwrap_or(&Value::Undefined))?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let c_path = match std::ffi::CString::new(p.clone()) {
            Ok(c) => c,
            Err(_) => return Err(ctx.make_error("Error", "path contains a NUL byte")),
        };
        let times = [timeval_from_secs(atime), timeval_from_secs(mtime)];
        // SAFETY: `c_path` is a valid NUL-terminated string; `times` is a 2-element array.
        let rc = unsafe {
            if follow {
                utimes(c_path.as_ptr(), times.as_ptr())
            } else {
                lutimes(c_path.as_ptr(), times.as_ptr())
            }
        };
        if rc != 0 {
            return Err(fs_error(ctx, op, &p, &std::io::Error::last_os_error()));
        }
        Ok(Value::Undefined)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (atime, mtime, follow, op);
        let err = ctx.make_error(
            "Error",
            format!("ENOSYS: {op} is not supported on this platform, {op} '{p}'"),
        );
        let _ = ctx.set_member(&err, "code", Value::str("ENOSYS"));
        Err(err)
    }
}

fn op_utimes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    set_times(ctx, args, true, "utime")
}

fn op_lutimes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    set_times(ctx, args, false, "lutime")
}

/// `statfs(path)` — filesystem statistics via `statvfs(3)`. Returns the fields Node's
/// `fs.statfs` reports (`type`, `bsize`, `blocks`, `bfree`, `bavail`, `files`, `ffree`);
/// `type` is 0 because `statvfs` carries no filesystem-type magic. Backed on macOS and Linux;
/// a clean ENOSYS error elsewhere. The trailing padding keeps our buffer at least as large as
/// the platform's `struct statvfs`, so the call can never write out of bounds.
#[cfg(target_os = "macos")]
#[repr(C)]
pub(crate) struct Statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u32,
    f_bfree: u32,
    f_bavail: u32,
    f_files: u32,
    f_ffree: u32,
    f_favail: u32,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    _pad: [u64; 8],
}
#[cfg(target_os = "linux")]
#[repr(C)]
pub(crate) struct Statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    _pad: [u64; 8],
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
extern "C" {
    pub(crate) fn statvfs(path: *const std::os::raw::c_char, buf: *mut Statvfs) -> i32;
}

fn op_statfs(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let c_path = match std::ffi::CString::new(p.clone()) {
            Ok(c) => c,
            Err(_) => return Err(ctx.make_error("Error", "path contains a NUL byte")),
        };
        let mut buf: std::mem::MaybeUninit<Statvfs> = std::mem::MaybeUninit::zeroed();
        // SAFETY: `c_path` is NUL-terminated; `buf` is a zeroed struct at least as large as the
        // platform's `struct statvfs` (extra trailing padding), so the write stays in bounds.
        let rc = unsafe { statvfs(c_path.as_ptr(), buf.as_mut_ptr()) };
        if rc != 0 {
            return Err(fs_error(
                ctx,
                "statfs",
                &p,
                &std::io::Error::last_os_error(),
            ));
        }
        let s = unsafe { buf.assume_init() };
        let o = Value::Obj(ctx.new_object());
        let set = |k: &str, v: f64, ctx: &mut Ctx| {
            let _ = ctx.set_member(&o, k, Value::Num(v));
        };
        set("type", 0.0, ctx);
        set("bsize", s.f_bsize as f64, ctx);
        set("blocks", s.f_blocks as f64, ctx);
        set("bfree", s.f_bfree as f64, ctx);
        set("bavail", s.f_bavail as f64, ctx);
        set("files", s.f_files as f64, ctx);
        set("ffree", s.f_ffree as f64, ctx);
        Ok(o)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let err = ctx.make_error(
            "Error",
            format!("ENOSYS: statfs is not supported on this platform, statfs '{p}'"),
        );
        let _ = ctx.set_member(&err, "code", Value::str("ENOSYS"));
        Err(err)
    }
}

/// One object of OS facts the JS `os` shim reads (snapshotted like Node's are). `hostname`
/// is separate because it can do I/O.
fn op_os_info(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let os_type = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows_NT",
        other => other,
    };
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let tmpdir = std::env::temp_dir().to_string_lossy().into_owned();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
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

// ---- node:zlib, over the shared DEFLATE codec ----

fn zlib_compress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Vec<u8>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray"));
    };
    ctx.make_uint8array(&codec(&bytes))
}

fn zlib_decompress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray"));
    };
    match codec(&bytes) {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

fn op_zlib_deflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::zlib_compress)
}
fn op_zlib_inflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::zlib_decompress)
}
fn op_zlib_deflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::deflate)
}
fn op_zlib_inflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::inflate)
}
fn op_zlib_gzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::gzip_compress)
}
fn op_zlib_gunzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::gzip_decompress)
}
fn op_zlib_brotli_compress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::brotli::brotli_compress)
}
fn op_zlib_brotli_decompress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::brotli::brotli_decompress)
}
fn op_zlib_zstd_compress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::zstd::zstd_compress)
}
fn op_zlib_zstd_decompress(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::zstd::zstd_decompress)
}
// ---- streaming deflate/inflate (`zlib.createGzip`, `createDeflate`, `createDeflateRaw`, …) ----

enum ZlibStream {
    Deflate(lumen_host::deflate::FramedDeflater),
    Inflate(lumen_host::deflate::FramedInflater),
}

/// Live streaming codecs, keyed by the id JS holds. A stream is released by `streamClose`
/// (the JS object's `close()`/`destroy()`), or leaks with the object — Node's does the same.
#[derive(Default)]
struct ZlibStreams {
    next: u64,
    streams: std::collections::HashMap<u64, ZlibStream>,
}

/// `(kind)` — `"deflate"`/`"inflate"` (raw), `"zlib"`/`"unzlib"` (zlib framing),
/// `"gzip"`/`"gunzip"`, or `"unzip"` (gzip or zlib by header); returns the stream id.
fn op_zlib_stream_open(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    use lumen_host::deflate::{FramedDeflater as D, FramedInflater as I, Framing};
    let kind = ctx
        .coerce_string(a.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let stream = match kind.as_str() {
        "deflate" => ZlibStream::Deflate(D::new(Framing::Raw)),
        "zlib" => ZlibStream::Deflate(D::new(Framing::Zlib)),
        "gzip" => ZlibStream::Deflate(D::new(Framing::Gzip)),
        "inflate" => ZlibStream::Inflate(I::new(Framing::Raw)),
        "unzlib" => ZlibStream::Inflate(I::new(Framing::Zlib)),
        "gunzip" => ZlibStream::Inflate(I::new(Framing::Gzip)),
        "unzip" => ZlibStream::Inflate(I::new(Framing::Auto)),
        other => {
            return Err(ctx.make_error("TypeError", format!("unknown zlib stream kind {other:?}")))
        }
    };
    let reg = ctx.host_mut::<ZlibStreams>().expect("registry");
    reg.next += 1;
    let id = reg.next;
    reg.streams.insert(id, stream);
    Ok(Value::Num(id as f64))
}

fn zlib_stream_id(a: &[Value]) -> u64 {
    a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64
}

fn zlib_stream_missing(ctx: &Ctx) -> Value {
    ctx.make_error("Error", "zlib stream is closed")
}

/// `(id, chunk)` — feed bytes; returns the bytes produced so far (a compressor emits a block
/// whenever enough input is buffered, otherwise only on flush).
fn op_zlib_stream_write(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = zlib_stream_id(a);
    let Some(bytes) = ctx.typed_array_bytes(a.get(1).unwrap_or(&Value::Undefined)) else {
        return Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray"));
    };
    let result = match ctx.host_mut::<ZlibStreams>().and_then(|r| r.streams.get_mut(&id)) {
        Some(ZlibStream::Deflate(d)) => Ok(d.write(&bytes)),
        Some(ZlibStream::Inflate(i)) => i.write(&bytes),
        None => return Err(zlib_stream_missing(ctx)),
    };
    match result {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(id, mode)` — a compressor flushes with zlib flush constant `mode` (`true` = `Z_FINISH`,
/// `false`/absent = `Z_SYNC_FLUSH`) and returns the bytes; a decompressor returns whether its
/// stream has ended (as a boolean).
fn op_zlib_stream_flush(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    use lumen_host::deflate::Flush;
    let id = zlib_stream_id(a);
    let mode = match a.get(1) {
        Some(Value::Bool(true)) => Flush::Finish,
        Some(Value::Num(n)) => Flush::from_zlib(*n as u32).unwrap_or(Flush::Sync),
        _ => Flush::Sync,
    };
    let result = match ctx.host_mut::<ZlibStreams>().and_then(|r| r.streams.get_mut(&id)) {
        Some(ZlibStream::Deflate(d)) => d.flush(mode),
        Some(ZlibStream::Inflate(i)) => return Ok(Value::Bool(i.finished())),
        None => return Err(zlib_stream_missing(ctx)),
    };
    match result {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(id)` — `zlib.reset()`: drop the window so the next message stands alone.
fn op_zlib_stream_reset(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = zlib_stream_id(a);
    match ctx.host_mut::<ZlibStreams>().and_then(|r| r.streams.get_mut(&id)) {
        Some(ZlibStream::Deflate(d)) => d.reset(),
        Some(ZlibStream::Inflate(i)) => i.reset(),
        None => return Err(zlib_stream_missing(ctx)),
    }
    Ok(Value::Undefined)
}

fn op_zlib_stream_close(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let id = zlib_stream_id(a);
    if let Some(reg) = ctx.host_mut::<ZlibStreams>() {
        reg.streams.remove(&id);
    }
    Ok(Value::Undefined)
}

/// `__zlib.crc32(bytes, seed)` — CRC-32 of `bytes`, optionally continued from `seed`.
fn op_zlib_crc32(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let v = a.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "zlib.crc32 expects a Buffer/TypedArray"));
    };
    let seed = a.get(1).and_then(|v| v.as_num_opt()).unwrap_or(0.0) as u32;
    Ok(Value::Num(
        lumen_host::deflate::crc32_from(seed, &bytes) as f64
    ))
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
