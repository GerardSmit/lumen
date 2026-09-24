//! A `node_modules` bundle on the Node runtime: `examples/node/main.mjs` imports a CommonJS
//! package (relative + JSON `require`s, an optional dependency behind `try`), an ES package with
//! a subpath export, and a module through a literal dynamic `import()`; `include_js!` follows
//! all of it (`node_modules = true`) and keeps the entry's function text (`keep_source`). The
//! JS asserts; this runs it from the blob with the fixture directory out of reach (the process
//! runs from the system temp directory).
//!
//! `cargo run -p lumen-aot --example aot_node [--release]`

use lumen_runtime::Runtime;

static APP: lumen::Precompiled = lumen_aot::include_js!(
    entry = "examples/node/main.mjs",
    node_modules = true,
    keep_source = "examples/node/*.mjs",
);

fn main() {
    // No JS may come from disk: run somewhere the fixture's node_modules cannot be found from.
    std::env::set_current_dir(std::env::temp_dir()).expect("chdir");
    let code = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            let mut runtime = Runtime::new();
            if let Err(e) = runtime.run_precompiled(&APP) {
                eprintln!("Uncaught {e}");
                return 1;
            }
            runtime.finish_process()
        })
        .expect("spawn")
        .join()
        .unwrap_or(1);
    std::process::exit(code);
}
