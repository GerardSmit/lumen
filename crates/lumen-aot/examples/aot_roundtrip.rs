//! Bytecode round-trip over real-world code: every `.js` file under the given paths (files or
//! directories, recursively; `node_modules` included) is precompiled with bytecode, loaded
//! back, and every attached chunk is compared with a fresh compile of the decoded function
//! (`lumen::precompiled::verify_bytecode`). Files that parse as neither a script nor a module
//! are skipped.
//!
//! `--run` also executes each file twice — once from source, once from its precompiled blob —
//! and requires identical completions (for self-contained scripts such as test262's harness).
//! `--time` prints, per file, the blob's AST decode / bytecode attach cost against compiling
//! every attached function afresh (the run-time compile work the blob saves at most).
//! `--load` times `Engine::load_precompiled` end to end (decode + run the top level, best of
//! 5) for the AST-only and the bytecode blob of each script, with the run-time compile counts;
//! `--then <js>` adds evaluating `<js>` after the load to the timed work (e.g. a TypeScript
//! `ts.transpile(...)` call against typescript.js, which tiers up hundreds of functions).
//!
//! `cargo run -p lumen-aot --release --example aot_roundtrip -- [--run] [--time] [--load]
//! <path>...`

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lumen::precompiled::{CompiledUnit, PrecompileBundle, SourceKind};
use lumen::{Completion, Engine, Precompiled};

fn collect(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if p
        .extension()
        .is_some_and(|x| x == "js" || x == "mjs" || x == "cjs")
    {
        out.push(p.to_path_buf());
    }
}

fn blob(src: &str, kind: SourceKind, bytecode: bool) -> Result<&'static [u8], String> {
    let unit = CompiledUnit::compile_with(src, kind, bytecode)?;
    let mut b = PrecompileBundle::new();
    b.add_compiled("main.js", unit)?;
    if kind == SourceKind::Module {
        b.set_entry("main.js")?;
    }
    Ok(Box::leak(b.finish().into_boxed_slice()))
}

fn completion(c: Result<Completion, lumen::ParseError>) -> String {
    match c {
        Ok(Completion::Value(v)) => format!("value {v}"),
        Ok(Completion::Throw { name, message }) => format!("throw {name}: {message}"),
        Err(e) => format!("error {}", e.message),
    }
}

/// Best-of-5 wall time of a fresh engine loading `blob`, and the compiles of the last load.
fn time_load(
    blob: &'static [u8],
    then: Option<&str>,
) -> (Duration, lumen::precompiled::PrecompiledStats, String) {
    let mut best = Duration::MAX;
    let (mut st, mut result) = (Default::default(), String::new());
    for _ in 0..5 {
        let mut e = Engine::new();
        lumen::precompiled::reset_stats();
        let t = Instant::now();
        result = completion(e.load_precompiled(&Precompiled::from_static(blob)));
        if let Some(js) = then {
            result = completion(e.eval(js, false));
        }
        best = best.min(t.elapsed());
        st = lumen::precompiled::stats();
    }
    (best, st, result)
}

fn main() {
    let mut run = false;
    let mut time = false;
    let mut load = false;
    let mut then: Option<String> = None;
    let mut want_then = false;
    let mut files = Vec::new();
    for a in std::env::args().skip(1) {
        if want_then {
            then = Some(a);
            want_then = false;
            continue;
        }
        match a.as_str() {
            "--then" => want_then = true,
            "--run" => run = true,
            "--time" => time = true,
            "--load" => load = true,
            p => collect(Path::new(p), &mut files),
        }
    }
    if files.is_empty() {
        eprintln!("usage: aot_roundtrip [--run] [--time] <file-or-dir>...");
        std::process::exit(2);
    }
    let (mut ok, mut skipped, mut failed) = (0usize, 0usize, Vec::new());
    let (mut functions, mut chunks, mut refusals) = (0usize, 0usize, 0usize);
    let (mut src_bytes, mut ast_bytes, mut bc_bytes) = (0usize, 0usize, 0usize);
    let (mut t_decode, mut t_attach, mut t_compile) =
        (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut runs, mut run_mismatch) = (0usize, Vec::new());
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            skipped += 1;
            continue;
        };
        let (kind, b) = match blob(&src, SourceKind::Script, true) {
            Ok(b) => (SourceKind::Script, b),
            Err(_) => match blob(&src, SourceKind::Module, true) {
                Ok(b) => (SourceKind::Module, b),
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            },
        };
        match lumen::precompiled::verify_bytecode(b) {
            Ok(r) => {
                ok += 1;
                functions += r.functions;
                chunks += r.chunks;
                refusals += r.refusals;
                t_decode += r.decode_ast;
                t_attach += r.attach;
                t_compile += r.recompile;
                src_bytes += src.len();
                let ast_only = blob(&src, kind, false).unwrap();
                ast_bytes += ast_only.len();
                bc_bytes += b.len() - ast_only.len();
                if time {
                    println!(
                        "{}: {} fns, {} chunks; decode {:?}, attach {:?}, compile-all {:?}",
                        f.display(),
                        r.functions,
                        r.chunks,
                        r.decode_ast,
                        r.attach,
                        r.recompile
                    );
                }
            }
            Err(e) => failed.push(format!("{}: {e}", f.display())),
        }
        if load && kind == SourceKind::Script {
            let ast_only = blob(&src, kind, false).unwrap();
            let (ta, sa, ra) = time_load(ast_only, then.as_deref());
            let (tb, sb, rb) = time_load(b, then.as_deref());
            println!(
                "{}: load AST-only {ta:?} ({} compiles); with bytecode {tb:?} ({} compiles, {} chunks decoded of {} registered); results {}",
                f.display(),
                sa.compiles,
                sb.compiles,
                sb.chunks_attached,
                sb.chunks_registered,
                if ra == rb { "agree" } else { "DIFFER" }
            );
            if then.is_some() {
                println!(
                    "  --then result: {}",
                    rb.chars().take(120).collect::<String>()
                );
            }
        }
        if run && kind == SourceKind::Script {
            runs += 1;
            let mut a = Engine::new();
            let from_src = completion(a.eval(&src, false));
            let mut e = Engine::new();
            let from_blob = completion(e.load_precompiled(&Precompiled::from_static(b)));
            // Precompiled functions print as NativeFunction; everything else must agree.
            if from_src != from_blob && !from_src.contains("function") {
                run_mismatch.push(format!(
                    "{}: source {from_src:?} vs blob {from_blob:?}",
                    f.display()
                ));
            }
        }
    }
    println!(
        "{} files: {ok} verified, {skipped} skipped (not JS / no parse), {} failed",
        files.len(),
        failed.len()
    );
    println!(
        "functions {functions}: {chunks} chunks identical to a fresh compile, {refusals} refusals \
         confirmed, {} left to run time",
        functions - chunks - refusals
    );
    println!(
        "bytes: source {src_bytes}, AST-only blobs {ast_bytes}, bytecode sections {bc_bytes} \
         (+{:.0}%)",
        100.0 * bc_bytes as f64 / ast_bytes.max(1) as f64
    );
    println!(
        "time: AST decode {t_decode:?}, bytecode attach {t_attach:?}, compile every attached \
         function {t_compile:?}"
    );
    if run {
        println!("runs: {runs} scripts, {} mismatches", run_mismatch.len());
        for m in &run_mismatch {
            println!("  MISMATCH {m}");
        }
    }
    for f in &failed {
        println!("  FAIL {f}");
    }
    if !failed.is_empty() || !run_mismatch.is_empty() {
        std::process::exit(1);
    }
}
