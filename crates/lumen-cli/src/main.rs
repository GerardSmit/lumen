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
//!   --max-old-space-size=MB            heap limit: past it scripts get a RangeError
//!   --timeout=MS (or LUMEN_TIMEOUT_MS)  stop the script after MS milliseconds (exit 124)

use std::io::{IsTerminal, Read};
use std::sync::atomic::{AtomicU64, Ordering};

use lumen_host::Completion;
use lumen_repl::Repl;
use lumen_runtime::Runtime;

mod aot;
mod aot_config;
mod aot_runtime_plan;
mod aot_transport;
mod dotenv;
mod options;
mod render;
mod typed;

/// The engine's size-class allocator: JS workloads are dominated by millions of short-lived
/// same-sized blocks (objects, scopes, strings), where the system allocator is slowest.
#[cfg(all(not(target_arch = "wasm32"), not(feature = "system-allocator")))]
#[global_allocator]
static GLOBAL_ALLOC: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;

/// The interpreter, the regex backtracker and the JSON/structured-clone walkers all recurse on
/// the native stack, and a debug build's frames are several KiB each — deep enough JS recursion
/// (well within the engine's own `MAX_EVAL_DEPTH` guard) overflows the default 8 MiB main-thread
/// stack before the guard fires. The reservation is virtual; pages are committed as touched.
const MAIN_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Writing past RLIMIT_FSIZE must fail with EFBIG, not kill the process with SIGXFSZ.
#[cfg(unix)]
fn ignore_sigxfsz() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;
    // SAFETY: installing SIG_IGN for a signal has no handler code to run.
    unsafe {
        signal(SIGXFSZ, SIG_IGN);
    }
}

fn main() {
    #[cfg(unix)]
    ignore_sigxfsz();
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
    let all_args: Vec<String> = std::env::args().collect();
    match lumen_os::embedded::blob() {
        Ok(Some(blob)) => {
            let mut runtime = Runtime::new();
            if let Err(message) = runtime.install_embedded_assets(blob) {
                die(1, &message);
            }
            runtime.set_process_args(&all_args[0], &[], &all_args);
            let native = blob
                .get(8..12)
                .is_some_and(|bytes| bytes == 7u32.to_le_bytes());
            let result = if native {
                runtime.run_native_owned(blob.into())
            } else {
                runtime.run_precompiled(&lumen::Precompiled::from_static(blob))
            };
            if let Err(message) = result {
                die(1, &message);
            }
            finish(&mut runtime);
            return;
        }
        Ok(None) => {}
        Err(message) => die(1, &message),
    }
    if all_args.get(1).map(String::as_str) == Some("record-profile")
        || (all_args.get(1).map(String::as_str) == Some("run")
            && all_args.iter().any(|arg| arg == "--record-profile"))
    {
        if let Err(message) = aot::record_profile(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("compile") {
        if let Err(message) = aot::compile(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("target") {
        if let Err(message) = aot::write_target(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("target-device") {
        if let Err(message) = aot_transport::target_device(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("upload") {
        if let Err(message) = aot_transport::upload(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("inventory") {
        if let Err(message) = aot_transport::inventory(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("pack-install") {
        if let Err(message) = aot::pack_install(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("sign-native") {
        if let Err(message) = aot::sign_native(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("symbolize-native") {
        if let Err(message) = aot::symbolize_native(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("runtime-imports") {
        if let Err(message) = aot::runtime_imports(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    if all_args.get(1).map(String::as_str) == Some("render") {
        if let Err(message) = render::run(&all_args[2..]) {
            die(1, &message);
        }
        return;
    }
    // `lumen typed ...`: the typed tier's analyzer / Node-compatible type stripper.
    if all_args.get(1).map(String::as_str) == Some("typed") {
        std::process::exit(typed::main(all_args[2..].to_vec()));
    }
    let argv0 = all_args
        .first()
        .cloned()
        .unwrap_or_else(|| "lumen".to_string());
    let mut args: Vec<String> = all_args.iter().skip(1).cloned().collect();
    if args.first().map(String::as_str) == Some("run") {
        args.remove(0);
        if args.is_empty() {
            die(1, "run requires an entry file");
        }
    }
    let force_repl = args.first().map(String::as_str) == Some("repl");
    if force_repl {
        args.remove(0);
    }

    let mut cli = options::Parsed::default();
    if let Err(e) = options::parse(&argv0, &args, false, &mut cli) {
        die(e.status, &e.message);
    }
    if cli.help {
        println!("usage: lumen-cli [repl | file.js [args...] | -e code] [--tier=interp|bytecode]");
        return;
    }
    if cli.version {
        println!("lumen {}", full_version());
        return;
    }

    load_env_files(&argv0, &cli);
    let mut opts = cli;
    if let Ok(env) = std::env::var("NODE_OPTIONS") {
        let mut from_env = options::Parsed::default();
        let parsed = options::split_node_options(&env)
            .and_then(|tokens| options::parse(&argv0, &tokens, true, &mut from_env));
        if let Err(e) = parsed {
            die(e.status, &e.message);
        }
        opts.merge_env(from_env);
    }

    let permission = opts.flag("--experimental-permission") || opts.flag("--permission");
    if !permission
        && (!opts.list("--allow-fs-read").is_empty() || !opts.list("--allow-fs-write").is_empty())
    {
        let flag = if opts.list("--allow-fs-read").is_empty() {
            "--allow-fs-write"
        } else {
            "--allow-fs-read"
        };
        die(
            1,
            &format!("{argv0}: --experimental-permission is required for {flag}"),
        );
    }
    if let Some(mode) = opts.string("--unhandled-rejections") {
        if !matches!(
            mode,
            "strict" | "warn" | "none" | "throw" | "warn-with-error-code"
        ) {
            die(
                9,
                &format!("{argv0}: invalid value for --unhandled-rejections"),
            );
        }
    }
    if opts.flag("--test") {
        let conflict = if opts.check {
            Some("--check")
        } else if opts.eval.is_some() {
            Some("--eval")
        } else if opts.interactive || force_repl {
            Some("--interactive")
        } else {
            None
        };
        if let Some(other) = conflict {
            die(
                9,
                &format!("{argv0}: either --test or {other} can be used, not both"),
            );
        }
        if opts.string("--watch-path").is_some() {
            die(
                9,
                &format!("{argv0}: --watch-path cannot be used in combination with --test"),
            );
        }
    }
    if opts.check && opts.eval.is_some() {
        die(
            9,
            &format!("{argv0}: either --check or --eval can be used, not both"),
        );
    }
    let module_input = match opts.string("--input-type") {
        None => false,
        Some("module") => true,
        Some("commonjs") => false,
        Some(other) => die(
            9,
            &format!("{argv0}: --input-type must be \"module\" or \"commonjs\", got \"{other}\""),
        ),
    };
    if module_input && opts.print {
        die(
            1,
            &format!("{argv0}: --print cannot be used with ESM input"),
        );
    }

    let tier = match opts.string("--tier") {
        None => None,
        Some("bytecode") => Some(lumen_host::Tier::Bytecode),
        Some("interp") => Some(lumen_host::Tier::Interp),
        Some(other) => die(2, &format!("unknown tier '{other}' (interp|bytecode)")),
    };
    let threshold = opts
        .string("--tier-threshold")
        .and_then(|n| n.parse::<u32>().ok());
    let expose_gc = opts.flag("--expose-gc");

    // `--test`: the operands are test files or directories, not a script.
    let test_runner = opts.flag("--test") && opts.eval.is_none();
    let file = if opts.eval.is_none() && !opts.stdin_dash && !test_runner {
        opts.rest.first().cloned()
    } else {
        None
    };
    let script_args: Vec<String> = match &file {
        Some(_) => opts.rest[1..].to_vec(),
        None if opts.stdin_dash => std::iter::once("-".to_string())
            .chain(opts.rest.iter().cloned())
            .collect(),
        None => opts.rest.clone(),
    };

    let mut runtime = Runtime::new();
    let script_argv: Vec<String> = match &file {
        Some(f) => {
            let script = std::path::absolute(f)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| f.clone());
            std::iter::once(script).chain(script_args).collect()
        }
        None => script_args,
    };
    runtime.set_process_args(&argv0, &opts.exec_argv, &script_argv);
    runtime.set_cli_options(&opts.options_json());
    if expose_gc {
        runtime.expose_gc();
    }
    if opts.flag("--expose-externalize-string") {
        runtime.expose_externalize_string();
    }
    if opts.flag("--trace-atomics-wait") {
        runtime.trace_atomics_wait();
    }
    if let Some(t) = tier {
        runtime.engine().set_tier(t);
    }
    if let Some(n) = threshold {
        runtime.engine().set_tier_threshold(n);
    }
    let heap_mb = match opts.string("--max-old-space-size") {
        None => None,
        Some(n) => match n.parse::<f64>() {
            Ok(mb) if mb > 0.0 => Some(mb),
            _ => die(
                2,
                &format!("{argv0}: invalid value for --max-old-space-size={n}"),
            ),
        },
    };
    let timeout_ms = match opts.string("--timeout") {
        Some(n) => match n.parse::<u64>() {
            Ok(ms) => Some(ms),
            Err(_) => die(2, &format!("{argv0}: invalid value for --timeout={n}")),
        },
        None => std::env::var("LUMEN_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok()),
    };
    if let Some(mb) = heap_mb {
        runtime
            .engine()
            .set_heap_limit((mb * 1024.0 * 1024.0) as usize);
    }
    if let Some(ms) = timeout_ms.filter(|&ms| ms > 0) {
        start_watchdog(&mut runtime, ms);
    }

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    run_prelude(
        &mut runtime,
        "typeof __lumenPrepareMain === 'function' && __lumenPrepareMain()",
    );
    run_preloads(&mut runtime, &opts, &cwd);

    if test_runner {
        run_source(&mut runtime, "__lumenRunTestMain()");
        return;
    }

    if opts.check {
        check_only(&mut runtime, file.as_deref(), module_input);
        return;
    }

    if opts.flag("--build-snapshot") {
        build_snapshot(&mut runtime, &opts, &argv0, file.as_deref());
        return;
    }
    if let Some(blob) = opts.string("--snapshot-blob") {
        if replay_snapshot(&mut runtime, &opts, &argv0, blob) {
            let args = js_string_array(&opts.rest);
            eval_checked(&mut runtime, &format!("{SNAPSHOT_CONTROL}.runMain({args})"));
            finish(&mut runtime);
            return;
        }
    }

    if let Some(code) = opts.eval.clone() {
        if module_input {
            run_module_text(&mut runtime, &code, &cwd, "[eval1]");
        } else {
            eval_global(&mut runtime, &code, "[eval]", opts.print);
        }
    } else if let Some(path) = file {
        // ESM vs CommonJS like Node: .mjs -> module, .cjs -> commonjs, .js -> the nearest
        // package.json "type". A module runs through the import graph; CJS as `require.main`.
        let result = if let Some(result) = aot::run_blob(&mut runtime, &path) {
            result
        } else if std::path::Path::new(&path).is_file() && is_esm_entry(&path) {
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
        finish(&mut runtime);
    } else if opts.interactive || force_repl || (std::io::stdin().is_terminal() && !opts.stdin_dash)
    {
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
        if module_input {
            run_module_text(&mut runtime, &src, &cwd, "[stdin]");
        } else {
            eval_global(&mut runtime, &src, "[stdin]", false);
        }
    }
}

const SNAPSHOT_MAGIC: &str = "LUMEN-SNAPSHOT 1";
const SNAPSHOT_CONTROL: &str = "require('v8').startupSnapshot[Symbol.for('lumen.snapshotControl')]";

/// The startup-snapshot emulation: a blob records the entry script and the V8 flags it was built
/// with; loading it replays that script (see `startupSnapshot` in stdlib_extras.js).
fn v8_flag_signature(opts: &options::Parsed) -> String {
    opts.exec_argv
        .iter()
        .filter(|a| a.starts_with("--harmony") || a.starts_with("--js-flags"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

fn js_string_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| js_string_literal(s)).collect();
    format!("[{}]", parts.join(","))
}

fn eval_checked(runtime: &mut Runtime, code: &str) -> String {
    match runtime.eval(code) {
        Ok(Completion::Value(v)) => v,
        Ok(Completion::Throw { name, message }) if name.is_empty() => {
            die(1, &format!("Uncaught {message}"))
        }
        Ok(Completion::Throw { name, message }) => die(1, &format!("Uncaught {name}: {message}")),
        Err(e) => die(1, &format!("SyntaxError: {} (line {})", e.message, e.line)),
    }
}

fn run_entry(runtime: &mut Runtime, path: &str) {
    let result = if let Some(result) = aot::run_blob(runtime, path) {
        result
    } else if is_esm_entry(path) {
        runtime.run_module(path)
    } else {
        runtime.run_main(path)
    };
    if let Err(e) = result {
        if lumen::typescript::is_node_uncaught_text(&e) {
            die(1, &e);
        }
        die(1, &format!("Uncaught {e}"));
    }
}

fn build_snapshot(runtime: &mut Runtime, opts: &options::Parsed, argv0: &str, file: Option<&str>) {
    let Some(entry) = file else {
        die(
            9,
            &format!("{argv0}: --build-snapshot must be used with an entry point script."),
        );
    };
    if entry == "node:embedded_snapshot_main" {
        die(
            9,
            &format!("{argv0}: Node.js was built without embedded snapshot"),
        );
    }
    let entry = absolute(entry);
    run_entry(runtime, &entry);
    eval_checked(runtime, &format!("{SNAPSHOT_CONTROL}.endBuild()"));
    let blob = opts
        .string("--snapshot-blob")
        .map(str::to_string)
        .unwrap_or_else(|| absolute("snapshot.blob"));
    let text = format!("{SNAPSHOT_MAGIC}\n{entry}\n{}\n", v8_flag_signature(opts));
    if let Err(e) = std::fs::write(&blob, text) {
        die(1, &format!("{argv0}: Cannot write {blob}: {e}"));
    }
    finish(runtime);
}

/// Replay a snapshot blob's entry script; true when it registered a deserialize main function.
fn replay_snapshot(runtime: &mut Runtime, opts: &options::Parsed, argv0: &str, blob: &str) -> bool {
    let text = std::fs::read_to_string(blob)
        .unwrap_or_else(|_| die(14, &format!("{argv0}: Cannot open {blob}")));
    let mut lines = text.lines();
    let (magic, entry, flags) = (lines.next(), lines.next(), lines.next().unwrap_or(""));
    let Some(entry) = entry.filter(|_| magic == Some(SNAPSHOT_MAGIC)) else {
        die(
            14,
            &format!("{argv0}: Failed to load the startup snapshot {blob}"),
        );
    };
    if flags != v8_flag_signature(opts) {
        die(14, &format!("{argv0}: Failed to load the startup snapshot {blob}: V8 flags differ from those it was built with"));
    }
    eval_checked(runtime, &format!("{SNAPSHOT_CONTROL}.beginReplay()"));
    run_entry(runtime, entry);
    eval_checked(runtime, &format!("String({SNAPSHOT_CONTROL}.endReplay())")) == "true"
}

/// Load each `--env-file` into the process environment. Variables already set in the real
/// environment win; a later file overrides an earlier one.
fn load_env_files(argv0: &str, cli: &options::Parsed) {
    let original: std::collections::HashSet<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    let optional: std::collections::HashSet<&String> =
        cli.list("--env-file-if-exists").iter().collect();
    let files = cli
        .list("--env-file")
        .iter()
        .chain(cli.list("--env-file-if-exists"));
    for path in files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) if optional.contains(path) => continue,
            Err(_) => die(9, &format!("{argv0}: {path}: not found")),
        };
        for (key, value) in dotenv::parse(&text) {
            if !original.contains(&key) {
                std::env::set_var(key, value);
            }
        }
    }
}

/// `-r` modules through the CommonJS loader, then `--import` modules through the ESM one.
fn run_preloads(runtime: &mut Runtime, opts: &options::Parsed, cwd: &str) {
    let required = opts.list("--require");
    if !required.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<String> = required
            .iter()
            .filter(|r| seen.insert(r.as_str()))
            .map(|r| js_string_literal(r))
            .collect();
        let src = format!("require('module')._preloadModules([{}]);", unique.join(","));
        run_prelude(runtime, &src);
    }
    let imports = opts.list("--import");
    if !imports.is_empty() {
        let src: String = imports
            .iter()
            .map(|i| format!("import {};\n", js_string_literal(i)))
            .collect();
        run_module_text(runtime, &src, cwd, "[preload]");
    }
}

fn run_prelude(runtime: &mut Runtime, src: &str) {
    match runtime.engine().eval(src, false) {
        Ok(Completion::Value(_)) => {}
        Ok(Completion::Throw { name, message }) => {
            if name.is_empty() {
                die(1, &format!("Uncaught {message}"));
            }
            die(1, &format!("Uncaught {name}: {message}"));
        }
        Err(e) => die(1, &format!("SyntaxError: {} (line {})", e.message, e.line)),
    }
}

fn run_module_text(runtime: &mut Runtime, src: &str, cwd: &str, name: &str) {
    let key = std::path::Path::new(cwd)
        .join(name)
        .to_string_lossy()
        .into_owned();
    if let Err(e) = runtime.run_module_source(src, &key) {
        die(1, &format!("Uncaught {e}"));
    }
    finish(runtime);
}

fn finish(runtime: &mut Runtime) {
    let code = runtime.finish_process();
    exit_if_timed_out();
    mem_report(runtime);
    runtime.exit(code);
}

/// `--check`: parse the script (a file, or stdin) without running it.
fn check_only(runtime: &mut Runtime, file: Option<&str>, module_input: bool) {
    let (name, source, module) = match file {
        Some(path) => {
            let resolved = resolve_script(path).unwrap_or_else(|| {
                die(
                    1,
                    &format!("Error: Cannot find module '{}'", absolute(path)),
                )
            });
            match std::fs::read_to_string(&resolved) {
                Ok(s) => (resolved.clone(), s, is_esm_entry(&resolved)),
                Err(_) => die(
                    1,
                    &format!("Error: Cannot find module '{}'", absolute(path)),
                ),
            }
        }
        None => {
            let mut src = String::new();
            if std::io::stdin().read_to_string(&mut src).is_err() {
                die(2, "cannot read stdin");
            }
            ("[stdin]".to_string(), src, module_input)
        }
    };
    let source = match source.strip_prefix("#!") {
        Some(rest) => format!("//{rest}"),
        None => source,
    };
    let wrapper = patched_wrapper(runtime);
    let result = match (&wrapper, module) {
        (Some((open, close)), false) => {
            lumen::check_script_syntax(&format!("{open}{source}{close}"))
        }
        _ => lumen::check_syntax(&source, module),
    };
    if let Err(e) = result {
        let text = source
            .lines()
            .nth((e.line as usize).saturating_sub(1))
            .unwrap_or("");
        die(
            1,
            &format!(
                "{name}:{}\n{text}\n\nSyntaxError: {}\n\nNode.js {}",
                e.line,
                e.message,
                full_version()
            ),
        );
    }
}

/// The file a script argument names: as given, with a registered extension appended, or a
/// directory's index file.
fn resolve_script(path: &str) -> Option<String> {
    const EXTENSIONS: [&str; 6] = [".js", ".json", ".node", ".cjs", ".mjs", ".ts"];
    let base = absolute(path);
    let file = std::path::Path::new(&base);
    if file.is_file() {
        return Some(base);
    }
    for ext in EXTENSIONS {
        let candidate = format!("{base}{ext}");
        if std::path::Path::new(&candidate).is_file() {
            return Some(candidate);
        }
    }
    if file.is_dir() {
        for ext in EXTENSIONS {
            let candidate = format!("{base}/index{ext}");
            if std::path::Path::new(&candidate).is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The `Module.wrapper` pair when a preload replaced it, else `None`.
fn patched_wrapper(runtime: &mut Runtime) -> Option<(String, String)> {
    let probe = "(function () { const w = require('module').wrapper; \
        if (w[0] === '(function (exports, require, module, __filename, __dirname) { ' && w[1] === '\\n});') return ''; \
        return String(w[0]) + '\\u0001' + String(w[1]); })()";
    let Ok(lumen::Completion::Value(pair)) = runtime.eval(probe) else {
        return None;
    };
    let (open, close) = pair.split_once('\u{1}')?;
    Some((open.to_string(), close.to_string()))
}

fn absolute(path: &str) -> String {
    std::path::absolute(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Run `code` as the global script Node's `-e`/`-p`/stdin evaluation is: `module`, `exports` and
/// `__filename` are globals, and every builtin module is a lazily loaded global.
fn eval_global(runtime: &mut Runtime, code: &str, name: &str, print: bool) {
    let setup = format!(
        "(function () {{ try {{ const M = require('module'); const m = new M({name}); \
         m.filename = process.platform === 'win32' ? require('path').join(process.cwd(), {name}) : process.cwd().replace(/\\/+$/, '') + '/' + {name}; \
         if (typeof M._nodeModulePaths === 'function') m.paths = M._nodeModulePaths(process.cwd()); \
         globalThis.module = m; globalThis.exports = m.exports; }} catch {{}} \
         globalThis.__filename = {name}; globalThis.__dirname = '.'; \
         try {{ for (const name of require('module').builtinModules) {{ \
           if (name.startsWith('_') || name.includes('/') || (name in globalThis && name !== 'fs')) continue; \
           const setReal = (v) => Object.defineProperty(globalThis, name, {{ value: v, writable: true, enumerable: true, configurable: true }}); \
           Object.defineProperty(globalThis, name, {{ get() {{ const v = require(name); \
             Object.defineProperty(globalThis, name, {{ get: () => v, set: setReal, enumerable: false, configurable: true }}); return v; }}, \
             set: setReal, enumerable: false, configurable: true }}); }} }} catch {{}} }})();",
        name = js_string_literal(name)
    );
    let _ = runtime.eval(&setup);
    let base = std::env::current_dir()
        .map(|dir| dir.join(name).to_string_lossy().into_owned())
        .unwrap_or_else(|_| name.to_string());
    runtime.install_module_loader(&base, true);
    if print {
        // Node's -p: the script's completion value is console.log'd at process exit.
        let wrapped = format!(
            "(function (r) {{ process.on(\"exit\", function () {{ console.log(r); }}); }})((0, eval)({}));",
            js_string_literal(code)
        );
        run_source(runtime, &wrapped);
    } else {
        run_source(runtime, code);
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
            exit_if_timed_out();
            mem_report(runtime);
            runtime.exit(code);
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

/// Share the runtime's root-field lookup; repository metadata can also contain `type`.
fn json_type_field(json: &str) -> Option<String> {
    lumen_runtime::package_type_from_json(json)
}

/// The `--timeout` budget in ms once it ran out (0 while it has not).
static TIMED_OUT_AFTER: AtomicU64 = AtomicU64::new(0);

/// How long after the interrupt the watchdog waits for the normal exit path before it exits the
/// process itself (a native call that cannot be interrupted would otherwise hang it).
const TIMEOUT_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// `--timeout`: a scheduler timer interrupts the runtime when the budget runs out. The script
/// stops at its next safe point, or the event loop wakes if it was blocked, and the CLI exits 124
/// (`timeout(1)`'s code) through its normal path with output flushed. Only if that path does not
/// finish within a few seconds does the watchdog exit the process itself.
fn start_watchdog(runtime: &mut Runtime, ms: u64) {
    let handle = runtime.interrupt_handle();
    lumen_os::sched::Deadline::start(
        "lumen-timeout",
        std::time::Duration::from_millis(ms),
        move || {
            TIMED_OUT_AFTER.store(ms, Ordering::SeqCst);
            handle.interrupt();
            lumen_os::sched::Deadline::start("lumen-timeout-exit", TIMEOUT_EXIT_GRACE, exit_if_timed_out)
                .detach();
        },
    )
    .detach();
}

fn exit_if_timed_out() {
    let ms = TIMED_OUT_AFTER.load(Ordering::SeqCst);
    if ms != 0 {
        eprintln!("lumen: script timed out after {ms} ms");
        std::process::exit(124);
    }
}

fn die(code: i32, message: &str) -> ! {
    exit_if_timed_out();
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
