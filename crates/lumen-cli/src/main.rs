//! lumen-cli — run JS on the lumen runtime, node/deno style.
//!
//! Usage:
//!   lumen-cli                          repl when stdin is a terminal, else eval stdin
//!   lumen-cli repl                     repl explicitly (even when piped — for scripting it)
//!   lumen-cli <file.js> [args...]      run a script to loop quiescence
//!   lumen-cli -e '<code>'              evaluate a string
//!   lumen-cli -p '<code>'              evaluate a string and print its result (Node's --print)
//!   lumen-cli -v | --version           print the version
//!   lumen-cli typed [--report|--json|--facts|--strip] FILE...   typed-tier analysis (see typed.rs)
//!   --tier=interp|bytecode, --tier-threshold=N   select the engine execution tier

use std::io::{IsTerminal, Read};

use lumen_host::Completion;
use lumen_repl::Repl;
use lumen_runtime::Runtime;

mod typed;

/// The engine's size-class allocator: JS workloads are dominated by millions of short-lived
/// same-sized blocks (objects, scopes, strings), where the system allocator is slowest.
#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL_ALLOC: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;

/// The interpreter, the regex backtracker and the JSON/structured-clone walkers all recurse on
/// the native stack, and a debug build's frames are several KiB each — deep enough JS recursion
/// (well within the engine's own `MAX_EVAL_DEPTH` guard) overflows the default 8 MiB main-thread
/// stack before the guard fires. The reservation is virtual; pages are committed as touched.
const MAIN_STACK_BYTES: usize = 256 * 1024 * 1024;

fn main() {
    let worker = std::thread::Builder::new()
        .name("lumen-main".to_string())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(real_main)
        .expect("spawn main thread");
    if worker.join().is_err() {
        std::process::exit(1);
    }
}

fn real_main() {
    let mut tier = None;
    let mut threshold = None;
    let mut eval_source = None;
    let mut print_result = false;
    let mut file = None;
    let mut force_repl = false;
    let mut expose_gc = false;
    // Runtime flags seen before the script, reported as `process.execArgv` (Node's split).
    let mut exec_argv = Vec::new();

    let all_args: Vec<String> = std::env::args().collect();
    // `lumen typed ...`: the typed tier's analyzer / Node-compatible type stripper.
    if all_args.get(1).map(String::as_str) == Some("typed") {
        std::process::exit(typed::main(all_args[2..].to_vec()));
    }
    let argv0 = all_args
        .first()
        .cloned()
        .unwrap_or_else(|| "lumen".to_string());
    let mut args = all_args.iter().skip(1).cloned();
    while let Some(a) = args.next() {
        if a.starts_with('-') && a != "-" {
            exec_argv.push(a.clone());
        }
        if let Some(t) = a.strip_prefix("--tier=") {
            tier = Some(match t {
                "bytecode" => lumen_host::Tier::Bytecode,
                "interp" => lumen_host::Tier::Interp,
                other => die(2, &format!("unknown tier '{other}' (interp|bytecode)")),
            });
        } else if let Some(n) = a.strip_prefix("--tier-threshold=") {
            threshold = n.parse::<u32>().ok();
        } else if a == "-e" || a == "--eval" {
            match args.next() {
                Some(code) => eval_source = Some(code),
                None => die(2, "-e expects code"),
            }
        } else if a == "-p" || a == "--print" || a == "-pe" || a == "-ep" {
            match args.next() {
                Some(code) => eval_source = Some(code),
                None => die(2, &format!("{a} expects code")),
            }
            print_result = true;
        } else if a == "repl" && file.is_none() {
            force_repl = true;
        } else if a == "-h" || a == "--help" {
            println!(
                "usage: lumen-cli [repl | file.js [args...] | -e code] [--tier=interp|bytecode]"
            );
            return;
        } else if a == "-v" || a == "--version" {
            println!("lumen {}", full_version());
            return;
        } else if a == "--expose-gc" || a == "--expose_gc" {
            expose_gc = true;
        } else if is_ignored_node_flag(&a) {
            // Accepted for Node command-line compatibility; lumen has no equivalent knob.
        } else if a.starts_with("--") {
            die(2, &format!("{argv0}: bad option: {a}"));
        } else {
            // First free arg is the script; the rest belong to it (visible via process.argv).
            file = Some(a);
            break;
        }
    }

    let mut runtime = Runtime::new();
    // process.argv is [binary, script, ...its args]: the runtime flags live in execArgv. Like
    // Node, the script is reported as an absolute path (`path.resolve`, symlinks kept).
    let script_argv: Vec<String> = match &file {
        Some(f) => {
            let script = std::path::absolute(f)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| f.clone());
            std::iter::once(script).chain(args).collect()
        }
        None => Vec::new(),
    };
    runtime.set_process_args(&argv0, &exec_argv, &script_argv);
    if expose_gc {
        runtime.expose_gc();
    }
    if let Some(t) = tier {
        runtime.engine().set_tier(t);
    }
    if let Some(n) = threshold {
        runtime.engine().set_tier_threshold(n);
    }

    if let Some(code) = eval_source {
        // Node's [eval] context: module/exports/__filename/__dirname are globals.
        let _ = runtime.eval(
            "(function () { try { const M = require('module'); const m = new M('[eval]'); \
             m.filename = require('path').join(process.cwd(), '[eval]'); \
             if (typeof M._nodeModulePaths === 'function') m.paths = M._nodeModulePaths(process.cwd()); \
             globalThis.module = m; globalThis.exports = m.exports; } catch {} \
             globalThis.__filename = '[eval]'; globalThis.__dirname = '.'; \
             try { for (const name of require('module').builtinModules) { \
               if (name.startsWith('_') || name.includes('/') || name in globalThis) continue; \
               const setReal = (v) => Object.defineProperty(globalThis, name, { value: v, writable: true, enumerable: true, configurable: true }); \
               Object.defineProperty(globalThis, name, { get() { const v = require(name); \
                 Object.defineProperty(globalThis, name, { get: () => v, set: setReal, enumerable: false, configurable: true }); return v; }, \
                 set: setReal, enumerable: false, configurable: true }); } } catch {} })();",
        );
        if print_result {
            // Node's -p: the script's completion value is console.log'd at process exit.
            let wrapped = format!(
                "(function (r) {{ process.on(\"exit\", function () {{ console.log(r); }}); }})((0, eval)({}));",
                js_string_literal(&code)
            );
            run_source(&mut runtime, &wrapped);
        } else {
            run_source(&mut runtime, &code);
        }
    } else if let Some(path) = file {
        if !std::path::Path::new(&path).is_file() {
            die(2, &format!("cannot read {path}: not a file"));
        }
        // ESM vs CommonJS like Node: .mjs -> module, .cjs -> commonjs, .js -> the nearest
        // package.json "type". A module runs through the import graph; CJS as `require.main`.
        let result = if is_esm_entry(&path) {
            runtime.run_module(&path)
        } else {
            runtime.run_main(&path)
        };
        if let Err(e) = result {
            // A rejected TypeScript entry prints as Node prints it (no `Uncaught` prefix).
            if lumen::typescript::is_node_uncaught_text(&e) {
                die(1, &e);
            }
            die(1, &format!("Uncaught {e}"));
        }
        let code = runtime.finish_process();
        mem_report(&mut runtime);
        if code != 0 {
            std::process::exit(code);
        }
    } else if force_repl || std::io::stdin().is_terminal() {
        println!(
            "lumen {} (.help for help, .exit or Ctrl-D to quit)",
            full_version()
        );
        let stdin = std::io::stdin();
        Repl::new(runtime).run(&mut stdin.lock(), &mut std::io::stdout());
    } else {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            die(2, "cannot read stdin");
        }
        run_source(&mut runtime, &src);
    }
}

/// `LUMEN_MEM_STATS=1`: the memory breakdown at a normal exit (see `lumen::memstats`).
fn mem_report(runtime: &mut Runtime) {
    if lumen::memstats::enabled() {
        runtime.engine().ctx().mem_report();
    }
}

/// `s` as a double-quoted JS string literal.
fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Evaluate + loop to quiescence; uncaught top-level throws exit 1 (console output already
/// streamed as the script ran).
fn run_source(runtime: &mut Runtime, src: &str) {
    match runtime.eval(src) {
        Ok(Completion::Value(_)) => {
            let code = runtime.finish_process();
            mem_report(runtime);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Ok(Completion::Throw { name, message }) => {
            if name.is_empty() {
                die(1, &format!("Uncaught {message}"));
            }
            die(1, &format!("Uncaught {name}: {message}"));
        }
        Err(e) => die(1, &format!("SyntaxError: {} (line {})", e.message, e.line)),
    }
}

/// Node's entry-point module-kind rule: `.mjs` is always ESM, `.cjs` always CommonJS, and
/// `.js` (or anything else) follows the nearest `package.json` `"type": "module"`.
fn is_esm_entry(path: &str) -> bool {
    let p = std::path::Path::new(path);
    match p.extension().and_then(|e| e.to_str()) {
        Some("mjs") => true,
        Some("cjs" | "cts") => false,
        Some("mts") => true,
        // JSX files use ESM imports (`import React …`); run them through the module graph so the
        // runtime's `.jsx` transpile hook applies.
        Some("jsx") => true,
        _ => nearest_package_type_is_module(p),
    }
}

fn nearest_package_type_is_module(file: &std::path::Path) -> bool {
    let mut dir = file.parent();
    while let Some(d) = dir {
        let pkg = d.join("package.json");
        if pkg.is_file() {
            return std::fs::read_to_string(&pkg)
                .ok()
                .and_then(|t| json_type_field(&t))
                .as_deref()
                == Some("module");
        }
        dir = d.parent();
    }
    false
}

/// Minimal scan for `"type": "..."` — the workspace ships no JSON crate.
fn json_type_field(json: &str) -> Option<String> {
    let mut rest = &json[json.find("\"type\"")? + 6..];
    rest = rest
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    rest.find('"').map(|end| rest[..end].to_string())
}

/// Node flags that tune V8 or Node internals lumen does not have: accepted and ignored so a
/// `package.json` script or a spawned `process.execPath` invocation written for Node still runs.
fn is_ignored_node_flag(a: &str) -> bool {
    const EXACT: &[&str] = &[
        "--no-warnings",
        "--no-deprecation",
        "--enable-source-maps",
        "--preserve-symlinks",
        "--trace-warnings",
        "--trace-uncaught",
        "--pending-deprecation",
        "--throw-deprecation",
        "--unhandled-rejections=strict",
        "--unhandled-rejections=throw",
        "--jitless",
        // Read back from process.execArgv by node:http (getOptionValue).
        "--insecure-http-parser",
    ];
    EXACT.contains(&a)
        || a.starts_with("--max-old-space-size")
        || a.starts_with("--max-semi-space-size")
        || a.starts_with("--stack-size")
        || a.starts_with("--experimental-")
        || a.starts_with("--no-experimental-")
        || a.starts_with("--disable-warning")
        || a.starts_with("--title=")
        || a.starts_with("--max-http-header-size=")
}

fn die(code: i32, message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(code);
}

/// `0.1.2`, or `0.1.2-nightly (abc1234)` when the build set LUMEN_VERSION_SUFFIX (the nightly
/// workflow passes `-nightly (<short sha>)`).
fn full_version() -> String {
    format!(
        "{}{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("LUMEN_VERSION_SUFFIX").unwrap_or("")
    )
}
