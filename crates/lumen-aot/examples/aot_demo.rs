//! End-to-end check of ahead-of-time compilation: a classic script plus an ES module graph
//! (with an import cycle, a re-export, a host-provided bare import and a dynamic `import()`)
//! are precompiled by `include_js!`, loaded from the blob, run, and checked. Then the example
//! reads its own executable and asserts that no JS source text (comments, source lines) made
//! it into the binary.
//!
//! The bundle is built twice — with precompiled bytecode (the default) and AST-only
//! (`bytecode = false`) — and both must produce identical results; the bytecode build must run
//! without a single bytecode compile at run time (every function's chunk comes from the blob).
//!
//! `cargo run -p lumen-aot --example aot_demo [--release]`

use lumen::precompiled::{reset_stats, stats};
use lumen::{Completion, Engine, Precompiled};

/// The whole bundle: the prelude script runs first, then the entry module.
static APP: Precompiled = lumen_aot::include_js!(
    script = "examples/js/prelude.js",
    entry = "examples/js/main.js",
    modules = ["examples/js/lib/extra.js"],
);

/// The same bundle without precompiled bytecode: functions compile at run time.
static APP_AST: Precompiled = lumen_aot::include_js!(
    script = "examples/js/prelude.js",
    entry = "examples/js/main.js",
    modules = ["examples/js/lib/extra.js"],
    bytecode = false,
);

/// A script on its own (the shorthand form).
static PRELUDE: Precompiled = lumen_aot::include_js!("examples/js/prelude.js");

fn eval(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false) {
        Ok(Completion::Value(v)) => v,
        Ok(Completion::Throw { name, message }) => panic!("{src}: threw {name}: {message}"),
        Err(e) => panic!("{src}: parse error {}", e.message),
    }
}

fn check(what: &str, got: String, want: &str) {
    assert_eq!(got, want, "{what}");
    println!("ok  {what} = {got}");
}

/// Markers are spelled backwards here so the example's own string constants cannot be what
/// the binary scan finds.
fn rev(s: &str) -> String {
    s.chars().rev().collect()
}

/// Load a bundle in a fresh engine (the host supplies only the bare import) and return it with
/// the bytecode-tier counters of the load and top-level run.
fn load_bundle(blob: &Precompiled) -> (Engine, lumen::precompiled::PrecompiledStats) {
    let mut e = Engine::new();
    // Only the bare specifier reaches the host; every `aot:/` module resolves in the blob.
    e.set_module_loader(|spec, referrer| {
        assert_eq!(
            spec, "host:greeting",
            "host loader asked for {spec} from {referrer}"
        );
        Some((
            "host:greeting".to_string(),
            "export function greet(who) { return 'hello ' + who; }".to_string(),
        ))
    });
    reset_stats();
    match e.load_precompiled(blob) {
        Ok(Completion::Value(_)) => {}
        Ok(Completion::Throw { name, message }) => panic!("bundle threw {name}: {message}"),
        Err(err) => panic!("bundle failed to load: {}", err.message),
    }
    (e, stats())
}

fn main() {
    println!(
        "blob sizes: bundle {} bytes ({} AST-only), prelude {} bytes",
        APP.as_bytes().len(),
        APP_AST.as_bytes().len(),
        PRELUDE.as_bytes().len()
    );

    // --- the script-only blob ---------------------------------------------------------------
    let mut e = Engine::new();
    match e.load_precompiled(&PRELUDE) {
        Ok(Completion::Value(v)) => check("prelude completion", v, "13"),
        Ok(Completion::Throw { name, message }) => panic!("prelude threw {name}: {message}"),
        Err(err) => panic!("prelude failed to load: {}", err.message),
    }
    check(
        "prelude global",
        eval(&mut e, "prelude.loaded"),
        "prelude-ok",
    );
    check(
        "script function toString",
        eval(&mut e, "prelude.scale.toString()"),
        "function scale() { [native code] }",
    );

    // --- the bundle -------------------------------------------------------------------------
    // AST-only first: the reference results, and proof the run-time tier-up path compiles.
    let (mut reference, ast_stats) = load_bundle(&APP_AST);
    println!("    AST-only bundle: {ast_stats:?}");
    assert_eq!(
        ast_stats.chunks_attached, 0,
        "no bytecode in the AST-only blob"
    );
    assert!(
        ast_stats.compiles > 0,
        "the AST-only bundle should compile at run time (the loop body tiers up on its first call)"
    );

    let (mut e, bc_stats) = load_bundle(&APP);
    println!("    bytecode bundle: {bc_stats:?}");
    assert!(
        bc_stats.chunks_registered >= 12,
        "precompiled chunks should be registered: {bc_stats:?}"
    );
    // Chunks decode when their function tiers up: `sumTo` (a loop) on its first call.
    assert!(
        bc_stats.chunks_attached >= 1,
        "the loop's chunk should be attached on its first call: {bc_stats:?}"
    );
    // Generators compile to bytecode now, so the bundle may carry no refusals at all.
    assert_eq!(
        bc_stats.compiles, 0,
        "loading and running the precompiled bundle must not compile anything"
    );
    println!(
        "ok  ran from precompiled bytecode: {} chunks registered, {} attached, {} refusals, 0 compiles",
        bc_stats.chunks_registered, bc_stats.chunks_attached, bc_stats.refusals_attached
    );
    // A function with a precompiled chunk tiers up on its first call (its body stays in the
    // blob), so calling it again attaches nothing new and compiles nothing.
    eval(&mut e, "for (let i = 0; i < 20; i++) prelude.scale(i); 0");
    let after = stats();
    assert_eq!(after.compiles, 0, "tier-up must use the chunk: {after:?}");
    assert!(
        after.chunks_attached >= bc_stats.chunks_attached,
        "attached chunks stay attached: {after:?}"
    );
    println!("ok  hot function runs on its precompiled chunk: {after:?}");
    let results = concat!(
        "JSON.stringify([out.sum, out.greeting, out.mulSrc, out.classSrc, out.metaUrl, ",
        "out.loop, out.closures, out.acc, out.gen])"
    );
    check(
        "bytecode and AST-only bundles agree",
        eval(&mut e, results),
        &eval(&mut reference, results),
    );
    check("loop", eval(&mut e, "out.loop"), "571571");
    check(
        "closures + constants",
        eval(&mut e, "out.closures"),
        "3,3,function,1000,true,dflt,1.5e-7",
    );
    check("class method", eval(&mut e, "out.acc"), "6");
    check("generator", eval(&mut e, "out.gen"), "1+2");
    check("sum across the cycle", eval(&mut e, "out.sum"), "66");
    check(
        "host bare import",
        eval(&mut e, "out.greeting"),
        "hello aot",
    );
    check(
        "module function toString",
        eval(&mut e, "out.mulSrc"),
        "function mul() { [native code] }",
    );
    check(
        "class toString",
        eval(&mut e, "out.classSrc"),
        "function Widget() { [native code] }",
    );
    check(
        "import.meta.url",
        eval(&mut e, "out.metaUrl"),
        "aot:/main.js",
    );
    eval(
        &mut e,
        "dyn.then(v => { globalThis.dynResult = v; }, e => { globalThis.dynResult = 'ERR ' + e; })",
    );
    check(
        "dynamic import inside the blob",
        eval(&mut e, "dynResult"),
        "extra:500",
    );
    check(
        "aot:/ specifier from a plain script",
        eval(
            &mut e,
            "import('aot:/lib/util.js').then(m => { globalThis.u = m.twice(21) }); 0",
        ),
        "0",
    );
    check("  … resolved", eval(&mut e, "u"), "42");
    // Errors thrown from precompiled code still carry name + message.
    check(
        "error from precompiled code",
        eval(
            &mut e,
            "try { prelude.scale(1n) } catch (err) { err.name + ': ' + (err.message.length > 0) }",
        ),
        "TypeError: true",
    );

    // A corrupted blob is a clean load error, not a crash.
    for (at, what) in [(16, "AST version"), (24, "bytecode layout fingerprint")] {
        let mut bad = APP.as_bytes().to_vec();
        bad[at] ^= 0xff;
        let bad: &'static [u8] = Box::leak(bad.into_boxed_slice());
        let err = Engine::new()
            .load_precompiled(&Precompiled::from_static(bad))
            .err()
            .expect("rejected");
        println!("ok  corrupted {what} rejected: {}", err.message);
    }
    // Every attached chunk equals a fresh compile of the decoded function.
    let report = lumen::precompiled::verify_bytecode(APP.as_bytes()).expect("bytecode verifies");
    println!(
        "ok  verified {} chunks + {} refusals over {} functions against fresh compiles",
        report.chunks, report.refusals, report.functions
    );

    // --- the binary holds no JS source --------------------------------------------------------
    let exe = std::env::current_exe().unwrap();
    let bin = std::fs::read(&exe).unwrap();
    let has = |needle: &str| bin.windows(needle.len()).any(|w| w == needle.as_bytes());
    let markers = [
        "7319_edulerp_TNEMMOC_AHPLA_REKRAM", // comments in every file
        "2144_TNEMMOC_KCOLB_ATEB_REKRAM",
        "0255_TNEMMOC_NIAM_AMMAG_REKRAM",
        "1688_TNEMMOC_HTAM_ATLED_REKRAM",
        "0922_TNEMMOC_LITU_NOLISPE_REKRAM",
        "4066_TNEMMOC_ARTXE_ATEZ_REKRAM",
        "1   +   3 * x nruter", // a source line with its odd spacing
        "{ )b ,a(lum noitcnuf", // a function's source text
        "ESAB + b * a nruter",
    ];
    // Sanity: the scan does find the blob's own data (a string literal of the program).
    assert!(
        has(&rev("ko-edulerp")),
        "scan sanity: the blob's string literal should be present"
    );
    for m in markers {
        let needle = rev(m);
        assert!(
            !has(&needle),
            "JS source text {needle:?} found in {}",
            exe.display()
        );
    }
    println!(
        "ok  {} ({} bytes) contains none of {} source markers",
        exe.display(),
        bin.len(),
        markers.len()
    );
    println!("all checks passed");
}
