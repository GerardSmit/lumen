//! Precompile the `node:` compat JS glue to an AST snapshot at build time (see
//! `lumen-web/build.rs` for the rationale — parsing the static glue on every boot is the
//! dominant cold-start cost). Assembles the same IIFE `lib.rs` used to `concat!` and writes the
//! source + snapshot to `OUT_DIR`, the single source of truth.
//!
//! The snapshot is an ahead-of-time blob (`lumen::precompiled::precompile_glue`): the glue's
//! AST, its bytecode, and its function text compressed (for `Function.prototype.toString`, which
//! shows a builtin's source as Node's JS builtins do) — no plain glue text in the binary.
//!
//! Files of optional builtins are left out when their cargo feature is off (see [`GATED`]).

use std::path::PathBuf;

/// Order matters: preamble first; each builtin registers itself into `__builtins` as it loads, so
/// dependencies must come first (events ← stream ← http; util/crypto/shims before their users);
/// module.js is LAST because it snapshots `__builtins.keys()` as the core-module set.
///
/// `wrap` puts a file's body in its own `{ }` block so its top-level `const`/`class` names stay
/// private (the whole glue is one IIFE, so unwrapped files share a scope and would collide). The
/// original files kept names globally unique; the newer module files reuse names (EventEmitter,
/// Readable, …) and rely on block isolation, talking to each other only through `__builtins`.
struct GlueFile {
    name: &'static str,
    wrap: bool,
}
const JS_FILES: &[GlueFile] = &[
    GlueFile {
        name: "preamble.js",
        wrap: false,
    },
    GlueFile {
        name: "buffer.js",
        wrap: false,
    },
    GlueFile {
        name: "path.js",
        wrap: false,
    },
    GlueFile {
        name: "os.js",
        wrap: false,
    },
    GlueFile {
        name: "events.js",
        wrap: true,
    },
    GlueFile {
        name: "diagnostics_channel.js",
        wrap: true,
    },
    GlueFile {
        name: "domain.js",
        wrap: true,
    },
    GlueFile {
        name: "trace_events.js",
        wrap: true,
    },
    GlueFile {
        name: "util.js",
        wrap: true,
    },
    GlueFile {
        name: "console.js",
        wrap: true,
    },
    GlueFile {
        name: "timers.js",
        wrap: true,
    },
    GlueFile {
        name: "crypto.js",
        wrap: true,
    },
    GlueFile {
        name: "punycode.js",
        wrap: true,
    },
    GlueFile {
        name: "shims.js",
        wrap: true,
    },
    GlueFile {
        name: "url.js",
        wrap: true,
    },
    GlueFile {
        name: "assert.js",
        wrap: true,
    },
    GlueFile {
        name: "stream.js",
        wrap: true,
    },
    // net.js is Node's net/http/https stack (generated; see its header).
    GlueFile {
        name: "net.js",
        wrap: true,
    },
    // fs.js runs Node's fs sources over `stream`/`events`/`url`/`os`, so it loads after them.
    GlueFile {
        name: "fs.js",
        wrap: true,
    },
    GlueFile {
        name: "http2_huffman.js",
        wrap: true,
    },
    GlueFile {
        name: "http2_codec.js",
        wrap: true,
    },
    GlueFile {
        name: "http2.js",
        wrap: true,
    },
    GlueFile {
        name: "child_process.js",
        wrap: true,
    },
    GlueFile {
        name: "dns.js",
        wrap: true,
    },
    GlueFile {
        name: "stdlib_extras.js",
        wrap: true,
    },
    GlueFile {
        name: "tty.js",
        wrap: true,
    },
    GlueFile {
        name: "tls.js",
        wrap: true,
    },
    // Loaded after stdlib_extras so the real implementation replaces its compatibility stub.
    GlueFile {
        name: "worker_threads.js",
        wrap: true,
    },
    GlueFile {
        name: "vm.js",
        wrap: true,
    },
    GlueFile {
        name: "repl.js",
        wrap: true,
    },
    GlueFile {
        name: "cluster.js",
        wrap: true,
    },
    GlueFile {
        name: "dgram.js",
        wrap: true,
    },
    GlueFile {
        name: "wasi.js",
        wrap: true,
    },
    GlueFile {
        name: "constants.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_ffi.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_jsc.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_sqlite.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_postgres.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_mysql.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_sql.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_redis.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_cookies.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_router.js",
        wrap: true,
    },
    GlueFile {
        name: "bun_s3.js",
        wrap: true,
    },
    GlueFile {
        name: "bun.js",
        wrap: true,
    },
    GlueFile {
        name: "typescript_strip.js",
        wrap: true,
    },
    // The builtins' ESM export names, for module.js.
    GlueFile {
        name: "esm_exports.js",
        wrap: false,
    },
    GlueFile {
        name: "module.js",
        wrap: false,
    },
    // After module.js: registers the node_modules-first fallback addons (bufferutil, ...), which
    // must stay out of the core-module set module.js snapshots.
    GlueFile {
        name: "addons.js",
        wrap: true,
    },
];

/// Glue files that run on first use rather than at startup (see preamble.js `__lazyGlue`): the
/// file becomes a closure, registered under the `__builtins`/`__internals` names it sets and the
/// globals it defines, and runs when one of them is first looked up. A file qualifies when its
/// top level only defines things and registers them — no patching of `process`, prototypes or
/// globals other than the ones it defines. Each lazy file costs nothing (not even its decoded
/// AST) until a program touches it, which is most of the node glue for most programs.
const LAZY: &[&str] = &[
    "trace_events.js",
    "util.js",
    "crypto.js",
    "punycode.js",
    "shims.js",
    "url.js",
    "assert.js",
    "stream.js",
    "net.js",
    "fs.js",
    "http2_huffman.js",
    "http2_codec.js",
    "http2.js",
    "child_process.js",
    "dns.js",
    "tty.js",
    "tls.js",
    "worker_threads.js",
    "vm.js",
    "repl.js",
    "cluster.js",
    "dgram.js",
    "wasi.js",
    "constants.js",
    "bun_ffi.js",
    "bun_jsc.js",
    "bun_sqlite.js",
    "bun_postgres.js",
    "bun_mysql.js",
    "bun_sql.js",
    "bun_redis.js",
    "bun_cookies.js",
    "bun_router.js",
    "bun_s3.js",
    "bun.js",
];

/// The names a lazy glue file registers, as `__lazyGlue` takes them: the `__builtins` names, the
/// `__internals` names, and the globals it defines at (near) top level — space-separated.
fn lazy_names(file: &str, body: &str) -> [String; 3] {
    let literal_args = |call: &str| -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find(call) {
            rest = &rest[at + call.len()..];
            let arg = rest.trim_start();
            let Some(arg) = arg.strip_prefix('"') else {
                panic!("{file}: {call}…) needs a string-literal name to load lazily (see LAZY)");
            };
            let name = &arg[..arg.find('"').expect("unterminated name")];
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        names
    };
    let builtins = literal_args("__builtins.set(");
    let internals = literal_args("__internals.set(");
    let mut globals: Vec<String> = Vec::new();
    for line in body.lines() {
        let indent = line.len() - line.trim_start().len();
        let line = line.trim_start();
        if indent > 2 {
            continue;
        }
        let name = if let Some(rest) = line.strip_prefix("Object.defineProperty(globalThis, \"") {
            rest.split('"').next()
        } else if let Some(rest) = line.strip_prefix("globalThis.") {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            rest[end..].trim_start().strip_prefix('=').filter(|r| !r.starts_with('=')).map(|_| &rest[..end])
        } else {
            None
        };
        if let Some(name) = name {
            if !globals.iter().any(|g| g == name) {
                globals.push(name.to_string());
            }
        }
    }
    assert!(
        !builtins.is_empty() || !internals.is_empty() || !globals.is_empty(),
        "{file}: a lazy glue file must register something (see LAZY)"
    );
    [builtins.join(" "), internals.join(" "), globals.join(" ")]
}

/// Glue files that belong to an optional cargo feature (`CARGO_FEATURE_<NAME>`), left out of
/// the glue when it is off.
const GATED: &[(&str, &str)] = &[
    ("http2_huffman.js", "HTTP2"),
    ("http2_codec.js", "HTTP2"),
    ("http2.js", "HTTP2"),
    ("cluster.js", "CLUSTER"),
    ("dgram.js", "DGRAM"),
    ("wasi.js", "WASI"),
    ("bun_ffi.js", "BUN"),
    ("bun_jsc.js", "BUN"),
    ("bun_sqlite.js", "BUN"),
    ("bun_postgres.js", "BUN"),
    ("bun_mysql.js", "BUN"),
    ("bun_sql.js", "BUN"),
    ("bun_redis.js", "BUN"),
    ("bun_cookies.js", "BUN"),
    ("bun_router.js", "BUN"),
    ("bun_s3.js", "BUN"),
    ("bun.js", "BUN"),
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest.join("src/js");

    let mut glue = String::from("(() => {\n");
    for (index, file) in JS_FILES.iter().enumerate() {
        let gate = GATED.iter().find(|(name, _)| *name == file.name);
        if gate.is_some_and(|(_, f)| std::env::var_os(format!("CARGO_FEATURE_{f}")).is_none()) {
            continue;
        }
        let path = src_dir.join(file.name);
        println!("cargo:rerun-if-changed={}", path.display());
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // Node's lib modules (`defineModule(name, function (module, exports, …) {…})`) each run
        // once too: mark them like the lazy files' bodies.
        let body = body.replace(
            "function (module, exports, require, internalBinding, primordials) {",
            "function (module, exports, require, internalBinding, primordials) {\"lumen:run-once\";",
        );
        // A memory checkpoint after each file, for LUMEN_MEM_STATS (see lumen-host).
        let mark = if std::env::var_os("LUMEN_GLUE_MEM_MARKS").is_some() {
            format!(
                "\nif (typeof __lumenMemMark === \"function\") __lumenMemMark({:?});\n",
                file.name
            )
        } else {
            String::new()
        };
        if LAZY.contains(&file.name) {
            assert!(file.wrap, "{}: a lazy glue file must be wrapped", file.name);
            let [builtins, internals, globals] = lazy_names(file.name, &body);
            // The `"lumen:run-once"` directive (lumen's `serialize::RUN_ONCE`): the body runs
            // once, on the tree-walker, and the collector can release it afterwards.
            glue.push_str(&format!(
                "__lazyGlue({index}, {builtins:?}, {internals:?}, {globals:?}, () => {{\n\"lumen:run-once\";\n"
            ));
            glue.push_str(&body);
            glue.push_str(&mark);
            glue.push_str("\n});\n");
            continue;
        }
        if index > 0 {
            // preamble.js (index 0) declares it.
            glue.push_str(&format!("__glueIndex = {index};\n"));
        }
        if file.wrap {
            glue.push_str("{\n");
            glue.push_str(&body);
            glue.push_str("\n}\n");
        } else {
            glue.push_str(&body);
        }
        glue.push_str(&mark);
    }
    // Registrations made after startup (by code the glue runs later) win over any lazy file's.
    glue.push_str("\n__glueIndex = 1e9;\n})();");

    let blob = lumen::precompiled::precompile_glue(&glue, "node-glue")
        .unwrap_or_else(|e| panic!("node glue failed to precompile: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // node_glue.js is for reference only (not linked).
    std::fs::write(out.join("node_glue.js"), &glue).unwrap();
    std::fs::write(out.join("node_glue.aot"), &blob).unwrap();

    std::fs::write(out.join("ffi_trampolines.rs"), generate_ffi_trampolines()).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LUMEN_GLUE_MEM_MARKS");
}

/// Maximum FFI argument count and the JSCallback thunk-pool size per arity. Kept in sync with the
/// `MAX_ARGS`/`JSCB_POOL` consts in `src/ffi.rs`.
const MAX_ARGS: usize = 8;
const JSCB_POOL: usize = 16;

/// Emit the libffi-free trampoline dispatch (`src/ffi.rs` `include!`s this).
///
/// # The ABI-class monomorphization
///
/// A native call whose argument register classes are only known at runtime is normally handled by
/// libffi. lumen has no libffi, so instead we *monomorphize*: for every register-class signature we
/// support, we emit a concrete `extern "C" fn(..) -> T` pointer type, `transmute` the resolved
/// symbol to it, and call it — letting the Rust/LLVM backend place each argument in the register the
/// platform ABI dictates.
///
/// The key trick that keeps this finite: on both x86-64 SysV and arm64 AAPCS, integer/pointer
/// arguments and floating-point arguments are assigned to *independent* register files (SysV
/// INTEGER vs SSE; AAPCS GPR/NGRN vs SIMD/NSRN). For register-class-only calls within the register
/// budget (≤8 of each here), an argument's home depends solely on how many prior arguments of the
/// *same* class there were — never on the interleaving. So an interleaved signature like
/// `f(int, double, int)` places its args identically to the canonical `f(int, int, double)`. The
/// caller therefore reorders actual arguments into "all integers first (in order), then all floats
/// (in order)", and we only enumerate canonical shapes: an integer count plus a float-width
/// sequence. That collapses 3^n interleavings to ~1000 shapes.
///
/// Floats still need per-position `f32`/`f64` types (they share a SIMD register but differ in the
/// bit pattern the callee reads), so the float suffix is enumerated over width bitmasks.
///
/// Returns fold to three kinds — integer/pointer/void returns all come back through `-> u64` (the
/// caller masks to the declared width; a `void` callee just leaves the register garbage we ignore),
/// plus `-> f32` and `-> f64`.
fn generate_ffi_trampolines() -> String {
    let mut s = String::new();
    s.push_str("// @generated by build.rs — bun:ffi trampoline dispatch. Do not edit.\n\n");

    // Win64 assigns argument registers by *position* (arg i -> RCX/RDX/R8/R9 or XMM0-3), so the
    // class-independent reordering described above is wrong there: it gets a positional table.
    let win64 = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    for (fname, rt) in [
        ("call_int", "u64"),
        ("call_f32", "f32"),
        ("call_f64", "f64"),
    ] {
        if win64 {
            emit_win64_trampoline(&mut s, fname, rt);
            continue;
        }
        s.push_str(&format!(
            "/// Trampoline into a native function returning the `{rt}` register class.\n\
             pub unsafe fn {fname}(f: *const core::ffi::c_void, ints: &[u64], floats: &[FArg], _fpos: u32) -> {rt} {{\n\
             \x20   match (ints.len(), floats.len(), fmask(floats)) {{\n"
        ));
        for i in 0..=MAX_ARGS {
            for fc in 0..=(MAX_ARGS - i) {
                for mask in 0u32..(1u32 << fc) {
                    let mut params = Vec::new();
                    let mut argx = Vec::new();
                    for a in 0..i {
                        params.push("u64".to_string());
                        argx.push(format!("ints[{a}]"));
                    }
                    for j in 0..fc {
                        if mask & (1 << j) != 0 {
                            params.push("f64".to_string());
                            argx.push(format!("floats[{j}].f64v()"));
                        } else {
                            params.push("f32".to_string());
                            argx.push(format!("floats[{j}].f32v()"));
                        }
                    }
                    s.push_str(&format!(
                        "        ({i}, {fc}, {mask}) => {{ let g: extern \"C\" fn({}) -> {rt} = core::mem::transmute(f); g({}) }}\n",
                        params.join(", "),
                        argx.join(", "),
                    ));
                }
            }
        }
        s.push_str(
            "        _ => unreachable!(\"ffi signature outside the supported register budget\"),\n",
        );
        s.push_str("    }\n}\n\n");
    }

    // JSCallback thunk pool: a fixed matrix of static `extern "C"` functions, one per (arity, slot),
    // each forwarding into the JS-re-entry dispatcher with its baked-in coordinates. All arguments
    // arrive as `u64` (integer/pointer register class only — float-arg callbacks are refused at
    // registration, so no float thunks are needed) and every thunk returns `u64`.
    let ncols = JSCB_POOL;
    for n in 0..=MAX_ARGS {
        for k in 0..ncols {
            let params: Vec<String> = (0..n).map(|a| format!("a{a}: u64")).collect();
            let argslice: Vec<String> = (0..n).map(|a| format!("a{a}")).collect();
            s.push_str(&format!(
                "extern \"C\" fn jscb_{n}_{k}({}) -> u64 {{ jscb_dispatch({n}, {k}, &[{}]) }}\n",
                params.join(", "),
                argslice.join(", "),
            ));
        }
    }
    s.push_str(
        "\n/// The raw address of thunk `(arity, slot)` — handed to native code as the callback pointer.\n\
         #[allow(clippy::fn_to_numeric_cast_any)]\n\
         pub fn jscb_thunk_ptr(n: usize, k: usize) -> *const core::ffi::c_void {\n\
         \x20   match (n, k) {\n",
    );
    for n in 0..=MAX_ARGS {
        for k in 0..ncols {
            s.push_str(&format!(
                "        ({n}, {k}) => jscb_{n}_{k} as *const core::ffi::c_void,\n"
            ));
        }
    }
    s.push_str("        _ => core::ptr::null(),\n    }\n}\n");
    s
}

/// Emit a Win64 (x86-64 Microsoft ABI) trampoline. Each of the first four argument *positions*
/// takes a GPR (RCX/RDX/R8/R9) or an XMM register (XMM0-3) by its own class; later arguments go
/// to 8-byte stack slots in order whatever their class. So a shape is just the arity plus a
/// 4-bit "is float" mask over the register positions: 95 shapes instead of the ~1000 SysV ones.
///
/// A float in a register position is passed as an `f64` carrying the argument's own bits (an
/// `f32` zero-extended): a `float` callee reads only the low 32 bits of the XMM register, so no
/// separate `f32`/`f64` shapes are needed. Stack-slot floats go as `u64` with the same bits (the
/// callee reads the low 4 or all 8 bytes of the slot). `win64_slots` (`src/ffi.rs`) rebuilds the
/// positional bit slots from the int/float streams and the float-position mask.
fn emit_win64_trampoline(s: &mut String, fname: &str, rt: &str) {
    s.push_str(&format!(
        "/// Trampoline into a native function returning the `{rt}` register class (Win64).\n\
         pub unsafe fn {fname}(f: *const core::ffi::c_void, ints: &[u64], floats: &[FArg], fpos: u32) -> {rt} {{\n\
         \x20   let (n, a) = win64_slots(ints, floats, fpos);\n\
         \x20   match (n, fpos & 15) {{\n"
    ));
    for n in 0..=MAX_ARGS {
        for mask in 0u32..(1u32 << n.min(4)) {
            let mut params = Vec::new();
            let mut argx = Vec::new();
            for p in 0..n {
                if p < 4 && mask & (1 << p) != 0 {
                    params.push("f64");
                    argx.push(format!("f64::from_bits(a[{p}])"));
                } else {
                    params.push("u64");
                    argx.push(format!("a[{p}]"));
                }
            }
            s.push_str(&format!(
                "        ({n}, {mask}) => {{ let g: extern \"C\" fn({}) -> {rt} = core::mem::transmute(f); g({}) }}\n",
                params.join(", "),
                argx.join(", "),
            ));
        }
    }
    s.push_str(
        "        _ => unreachable!(\"ffi signature outside the supported register budget\"),\n",
    );
    s.push_str("    }\n}\n\n");
}
