//! Puppeteer as one executable: `js/test.mjs` and every module it reaches — puppeteer-core,
//! ws (CommonJS), @puppeteer/browsers, chromium-bidi, … — precompiled into the binary by
//! `include_js!` and run on the lumen Node runtime. No JavaScript is read from disk at run time.
//!
//!   pptr-aot            run js/test.mjs (launch the browser headless, drive a page, close)
//!   pptr-aot --stats    blob size, units, kept function text
//!   pptr-aot --scan     check the executable holds no JS source outside kept function bodies
//!
//! `PPTR_EXE` picks the browser; `PPTR_IMPORT_ONLY=1` stops right after the module graph has
//! been loaded and evaluated (startup measurement).

use std::time::Instant;

use lumen::precompiled::{kept_sources, list_units, SourceKind};
use lumen_runtime::Runtime;

/// The program. `node_modules = true` follows the bare imports (`puppeteer-core`, `ws`,
/// `@puppeteer/browsers`, …) into `node_modules` and bundles what they reach — literal dynamic
/// `import()`s included (the WebSocket transport, `LaunchOptions`, the BiDi mapper). Puppeteer
/// ships functions to the page as `fn.toString()` text: our own `evaluate` callbacks (js/) and
/// puppeteer-core's helpers keep their function source; nothing else does.
static APP: lumen::Precompiled = lumen_aot::include_js!(
    entry = "js/test.mjs",
    node_modules = true,
    keep_source = ["js/**", "puppeteer-core/**"],
);

/// The engine's size-class allocator, as the `lumen` CLI uses: startup alone (the Node glue, the
/// blob's ASTs) is millions of small same-sized blocks, where the system allocator is slowest.
#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL_ALLOC: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;

const MAIN_STACK_BYTES: usize = 256 * 1024 * 1024;

fn main() {
    let t0 = Instant::now();
    let worker = std::thread::Builder::new()
        .name("lumen-main".to_string())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(move || real_main(t0))
        .expect("spawn main thread");
    match worker.join() {
        Ok(code) => std::process::exit(code),
        Err(_) => std::process::exit(1),
    }
}

fn real_main(t0: Instant) -> i32 {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--stats") => return stats(),
        Some("--scan") => return scan(),
        Some("--dump-kept") => {
            // The kept function text of the units whose key contains the argument.
            let want = args.get(2).cloned().unwrap_or_default();
            for (key, text) in kept_sources(APP.as_bytes()).expect("valid blob") {
                if key.contains(&want) {
                    println!("==== {key}\n{text}");
                }
            }
            return 0;
        }
        _ => {}
    }
    let mut runtime = Runtime::new();
    runtime.set_process_args(&args[0], &[], &[]);
    let result = runtime.run_precompiled(&APP);
    if std::env::var_os("PPTR_AOT_TIMING").is_some() {
        eprintln!("[pptr-aot] total {:?}", t0.elapsed());
    }
    if let Err(e) = result {
        eprintln!("Uncaught {e}");
        return 1;
    }
    let code = runtime.finish_process();
    if lumen::memstats::enabled() {
        runtime.engine().ctx().mem_report();
    }
    code
}

fn stats() -> i32 {
    let blob = APP.as_bytes();
    let units = list_units(blob).expect("valid blob");
    let count = |k: SourceKind| units.iter().filter(|(kind, _)| *kind == k).count();
    let kept = kept_sources(blob).expect("valid blob");
    let kept_bytes: usize = kept.iter().map(|(_, t)| t.len()).sum();
    println!("blob: {} bytes", blob.len());
    println!(
        "units: {} ({} ES modules, {} CommonJS)",
        units.len(),
        count(SourceKind::Module),
        count(SourceKind::CommonJs)
    );
    println!(
        "kept function text: {} units, {kept_bytes} bytes uncompressed",
        kept.len()
    );
    0
}

/// Spelled backwards so this function's own string constants cannot be what the scan finds.
fn rev(s: &str) -> String {
    s.chars().rev().collect()
}

fn scan() -> i32 {
    let exe = std::env::current_exe().expect("current_exe");
    let bytes = std::fs::read(&exe).expect("read own executable");
    let mut failures = 0;
    // Text that sits OUTSIDE every function in the bundled files: file-header license comments
    // (every puppeteer-core module), trailing source-map comments, and a top-level comment of
    // js/test.mjs. None may be in the binary.
    let absent = [
        rev("reifitnedI-esneciL-XDPS"),
        rev("=LRUgnippaMecruos"),
        rev("hparg eludom elohw eht :tnemerusaem putratS"),
        rev("seirrac yranib toa-rtpp eht tpircs ehT"),
    ];
    for m in &absent {
        let n = bytes.windows(m.len()).filter(|w| *w == m.as_bytes()).count();
        println!("{:<5} binary: {m:?} x{n}", if n == 0 { "ok" } else { "FAIL" });
        failures += (n != 0) as usize;
    }
    // The kept function text (decompressed from the blob) holds function bodies only.
    let kept = kept_sources(APP.as_bytes()).expect("valid blob");
    for m in &absent {
        let hits: Vec<&str> = kept
            .iter()
            .filter(|(_, t)| t.contains(m.as_str()))
            .map(|(k, _)| k.as_str())
            .collect();
        println!(
            "{:<5} kept text: {m:?} in {} units {:?}",
            if hits.is_empty() { "ok" } else { "FAIL" },
            hits.len(),
            &hits[..hits.len().min(3)]
        );
        failures += !hits.is_empty() as usize;
    }
    // ... and does hold what Puppeteer sends to the page.
    let present = [
        rev("tnetnoCtxet.le >= )le("),
        rev("b * a >= )b ,a("),
    ];
    for m in &present {
        let found = kept.iter().any(|(_, t)| t.contains(m.as_str()));
        println!("{:<5} kept text has {m:?}", if found { "ok" } else { "FAIL" });
        failures += !found as usize;
    }
    let only_kept: Vec<&String> = kept
        .iter()
        .map(|(k, _)| k)
        .filter(|k| !(k.contains("/puppeteer-core/") || k.starts_with("aot:/js/")))
        .collect();
    println!(
        "{:<5} kept text only for js/** and puppeteer-core/** ({} units; others: {:?})",
        if only_kept.is_empty() { "ok" } else { "FAIL" },
        kept.len(),
        only_kept
    );
    failures += !only_kept.is_empty() as usize;
    if failures == 0 {
        println!("scan passed");
        0
    } else {
        println!("scan FAILED ({failures})");
        1
    }
}
