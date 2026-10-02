//! Ahead-of-time compiled JavaScript for lumen embedders: ship a binary that contains the
//! program, not its source.
//!
//! ```ignore
//! static APP: lumen::Precompiled = lumen_aot::include_js!(
//!     entry = "js/app.mjs",
//!     node_modules = true,                 // bundle puppeteer-core, ws, @puppeteer/browsers…
//!     keep_source = ["js/**", "puppeteer-core/**"], // fn.toString() text they send to the page
//! );
//!
//! // Under the lumen runtime (node:* builtins, require): see lumen-runtime's
//! // `Runtime::run_precompiled`. On a bare engine:
//! let mut engine = lumen::Engine::new();
//! engine.set_module_loader(host_loader); // whatever the blob does not contain (node:*)
//! engine.load_precompiled(&APP)?;
//! ```
//!
//! Install lumen's allocator in the binary, as the `lumen` shell does: on the system allocator
//! the V8 v7 benchmarks run up to 13% slower.
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;
//! ```
//!
//! # `include_js!`
//! Paths are relative to the invoking crate's `CARGO_MANIFEST_DIR`. Forms:
//! - `include_js!("app.js")` — one classic script.
//! - `include_js!(entry = "main.js")` (alias `module = …`) — an ES module entry; every module
//!   reachable from it through relative static imports/re-exports and literal dynamic
//!   `import("./x.js")` calls (`./`, `../`; exact path, then `.js`/`.mjs`/`.cjs`/`.json`, then a
//!   directory's `package.json` `main` / `index.js`) is bundled and keyed
//!   `aot:/<path from the bundle root>`. A computed `import(expr)` cannot be followed: list its
//!   targets in `modules`. Bare specifiers and attribute imports are left to the host's module
//!   loader unless `node_modules = true`.
//! - Any combination of `script = "a.js" | ["a.js", …]` (scripts, run first, in order),
//!   `entry = "…"`, `modules = ["…", …]` (extra modules, e.g. computed dynamic `import()`
//!   targets), `walk = false` (take the listed files only — an explicit file list),
//!   `root = "dir"` (the directory keys are relative to; default: the deepest directory holding
//!   every bundled module), `bytecode = false` (AST only: functions compile at run time), and:
//! - `node_modules = true` — also bundle bare package specifiers (`ws`, `@puppeteer/browsers`,
//!   `pkg/sub`), found by Node's `node_modules` walk from the importing file and resolved
//!   through `package.json` `exports` (conditions `node`, `import` / `require`, `default`;
//!   subpath patterns) or `main`. ES modules and CommonJS alike: a CommonJS file (by
//!   `.cjs`, or a `.js` outside a `"type": "module"` package) becomes a CommonJS unit —
//!   its body in Node's module wrapper — which the runtime's `require` loads from the blob,
//!   and literal `require("…")` calls in it are followed too (unresolvable ones, like optional
//!   native add-ons behind `try`, are left to run time). An `import` of a CommonJS file links
//!   to a synthesized facade module (`default` = `module.exports`, plus the names a static
//!   scan finds). JSON files become CommonJS units (`module.exports = JSON.parse(…)`). Node
//!   builtins (`node:*`, `fs`, …) are never bundled. Every resolved specifier is recorded in
//!   the blob, so at run time an `aot:/` module's `import "ws"` resolves inside the blob with
//!   no `node_modules` on disk.
//! - `keep_source = true | "glob" | ["glob", …]` — keep the exact source text of the functions
//!   and classes of matching files, so `Function.prototype.toString` works (Puppeteer
//!   serializes every `page.evaluate` callback and many of its own helpers with it). Globs:
//!   `**` any number of segments, `*`/`?` within one, matched at any directory boundary of the
//!   path relative to `CARGO_MANIFEST_DIR` (`"puppeteer-core/**"`); `true` = every file.
//!   Only function/class text is kept — module-level code and comments between functions are
//!   not — compressed, and decompressed only on the first `toString` that reads it.
//!   **Tradeoff:** kept text is recoverable from the binary; keep it only
//!   where `toString` is needed (the user's own `evaluate` callbacks live in the user's
//!   bundle, so its files need it too).
//! - `exclude = ["glob", …]` — never bundle matching files (their imports go to the host
//!   loader at run time), e.g. to drop an optional dynamic-import target.
//!
//! Every file is parsed eagerly at compile time, so a syntax error anywhere in the bundle is a
//! compile error. The expansion is a `lumen::Precompiled` constant expression holding the blob
//! (a byte-string literal) — see [`lumen::precompiled`] for the format.
//!
//! # Precompiled bytecode
//! Every function the bytecode compiler accepts is compiled at build time and its chunk stored
//! next to the AST; loading attaches it to the function, so the first call runs on the VM with
//! no compile at run time. Functions the compiler refuses run on the tree-walker as usual.
//! `lumen::precompiled::stats()` counts attached chunks and run-time compiles.
//!
//! # No source text in the binary
//! The blob holds the parsed AST with all function source text stripped
//! (`Function.prototype.toString()` of precompiled code returns
//! `function name() { [native code] }`). String/template/regex literals and identifiers remain
//! (they are the program's data); comments, whitespace and function text do not — except the
//! function/class text of files matched by `keep_source` (see above).
//!
//! Rebuild tracking: a proc macro cannot declare file dependencies on stable Rust
//! (`proc_macro::tracked_path` is unstable), so the expansion also contains, per input file,
//! `const _: &[u8] = include_bytes!("<absolute path>");`. That makes cargo/rustc record the
//! file as a dependency of the crate (edits trigger a rebuild) — and since an unreferenced
//! `const` item is never code-generated, its bytes never reach the object file (checked by
//! this crate's `aot_demo` example, which greps its own executable for the source). If you
//! need that as a hard guarantee rather than a property of rustc's codegen, use [`build`] from
//! a build script instead: it tracks inputs with `cargo:rerun-if-changed` and the binary only
//! ever `include_bytes!`s the blob.
//!
//! Loading checks the blob's container format, AST codec version, bytecode layout fingerprint
//! and lumen version; the expansion also asserts at compile time that the macro's lumen and the
//! one this crate links agree on all of them.

pub use lumen::precompiled::Precompiled;
pub use lumen_aot_macros::include_js;

#[path = "walk.rs"]
mod walk;
mod assets;

/// Precompile from a build script (`[build-dependencies] lumen-aot = …`), writing the blob to
/// a file — normally in `OUT_DIR` — and printing `cargo:rerun-if-changed` for every input.
/// Pick it up with [`include_precompiled!`](crate::include_precompiled):
///
/// ```ignore
/// // build.rs
/// fn main() {
///     let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("app.aot");
///     lumen_aot::build::precompile_to(&out, "js/main.js").unwrap();
/// }
/// // main.rs
/// static APP: lumen::Precompiled = lumen_aot::include_precompiled!("app.aot");
/// ```
#[cfg(feature = "native")]
pub mod native;

pub mod build {
    use std::path::{Path, PathBuf};

    pub use crate::walk::Spec;
    pub use crate::walk::Bundle;

    /// Compile a bundle without writing an output file.
    pub fn compile(base: impl AsRef<Path>, spec: &Spec) -> Result<Bundle, String> {
        crate::walk::bundle(base.as_ref(), spec)
    }

    /// Return every source and package manifest read by the bundle walker.
    /// Relative paths in `spec` resolve against `base`.
    pub fn collect_inputs(base: impl AsRef<Path>, spec: &Spec) -> Result<Vec<PathBuf>, String> {
        let bundle = crate::walk::bundle(base.as_ref(), spec)?;
        for warning in &bundle.warnings {
            eprintln!("warning: {warning}");
        }
        Ok(bundle.inputs)
    }

    /// Write a host-tool bundle, rejecting outputs that alias any bundled source.
    /// Relative input paths resolve against `base`, not the caller's environment.
    pub fn compile_to(
        base: impl AsRef<Path>,
        spec: &Spec,
        out: impl AsRef<Path>,
    ) -> Result<usize, String> {
        let bundle = crate::walk::bundle(base.as_ref(), spec)?;
        for warning in &bundle.warnings {
            eprintln!("warning: {warning}");
        }
        let out = out.as_ref();
        let report = out.with_extension("trim.txt");
        if spec.trim_modules && report == out {
            return Err("output and trim report paths must differ".into());
        }
        for path in std::iter::once(out).chain(spec.trim_modules.then_some(report.as_path())) {
            if let Ok(output) = path.canonicalize() {
                if bundle.inputs.iter().any(|p| p.canonicalize().is_ok_and(|p| p == output)) {
                    return Err("output would overwrite a bundled source".into());
                }
            }
        }
        if spec.trim_modules {
            let paths = bundle.inputs.iter().map(|p| p.canonicalize()
                .map_err(|e| format!("{}: {e}", p.display())))
                .collect::<Result<Vec<_>, _>>()?;
            let mut root = paths.first().and_then(|p| p.parent()).unwrap_or(Path::new(".")).to_path_buf();
            while paths.iter().any(|path| !path.starts_with(&root)) {
                if !root.pop() { return Err("bundle sources have no common root".into()); }
            }
            let mut lines = vec!["trim: modules".to_owned(),
                format!("output bytes: {}", bundle.blob.len()),
                "Module roots and their transitive imports are kept; no member analysis is applied.".into()];
            for path in &paths {
                lines.push(format!("kept input: {}", path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")));
            }
            for name in &bundle.required_modules { lines.push(format!("required native: {name}")); }
            for warning in &bundle.warnings { lines.push(format!("warning: {warning}")); }
            std::fs::write(&report, format!("{}\n", lines.join("\n")))
                .map_err(|e| format!("{}: {e}", report.display()))?;
        }
        std::fs::write(out, &bundle.blob).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(bundle.blob.len())
    }

    /// Bundle the ES module graph rooted at `entry` (see [`precompile_spec`]).
    pub fn precompile_to(out: impl AsRef<Path>, entry: impl AsRef<Path>) -> Result<(), String> {
        precompile_spec(
            out,
            &Spec {
                entry: Some(entry.as_ref().to_path_buf()),
                walk: true,
                ..Spec::default()
            },
        )
    }

    /// Bundle `spec` (paths relative to `CARGO_MANIFEST_DIR`, else the current directory) and
    /// write the blob to `out`.
    pub fn precompile_spec(out: impl AsRef<Path>, spec: &Spec) -> Result<(), String> {
        let base = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_default();
        let bundle = crate::walk::bundle(&base, spec)?;
        for warning in &bundle.warnings {
            println!("cargo:warning={warning}");
        }
        for input in &bundle.inputs {
            println!("cargo:rerun-if-changed={}", input.display());
        }
        std::fs::write(out.as_ref(), &bundle.blob)
            .map_err(|e| format!("{}: {e}", out.as_ref().display()))
    }
}

/// A blob a build script wrote to `OUT_DIR` with [`build`], as a [`Precompiled`].
#[macro_export]
macro_rules! include_precompiled {
    ($name:literal) => {
        $crate::Precompiled::from_static(include_bytes!(concat!(env!("OUT_DIR"), "/", $name)))
    };
}

/// Compile-time agreement check emitted by `include_js!`: the blob's container and AST
/// versions and bytecode layout fingerprint (from the lumen the macro ran) must match the lumen
/// this crate links.
#[doc(hidden)]
pub const fn __check_versions(format: u32, ast: u32, layout: u64) {
    assert!(
        format == lumen::precompiled::FORMAT_VERSION
            && ast == lumen::precompiled::AST_VERSION
            && layout == lumen::precompiled::LAYOUT_FINGERPRINT,
        "include_js!: lumen-aot-macros and lumen disagree on the precompiled format; use one lumen version"
    );
}
