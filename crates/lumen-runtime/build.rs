//! Precompile the runtime's embedded JS (see `src/js/`) to AOT blobs at build time, as
//! lumen-node and lumen-web do for their glue, so booting a realm decodes it instead of
//! lexing and parsing it.

use std::path::PathBuf;

const SCRIPTS: &[&str] = &[
    "process",
    "env_proxy",
    "error_shim",
    "worker",
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for name in SCRIPTS {
        let path = manifest.join("src/js").join(format!("{name}.js"));
        println!("cargo:rerun-if-changed={}", path.display());
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let blob = lumen::precompiled::precompile_glue(&src, name)
            .unwrap_or_else(|e| panic!("{name}.js failed to precompile: {e}"));
        std::fs::write(out.join(format!("{name}.aot")), blob).unwrap();
    }
    println!("cargo:rerun-if-changed=build.rs");
}
