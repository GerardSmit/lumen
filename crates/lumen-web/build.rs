use std::{env, fs, path::PathBuf};

fn main() {
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../lumen-host/src/performance_timeline.js");
    println!("cargo:rerun-if-changed={}", source.display());
    let glue = fs::read_to_string(&source).expect("read shared Performance facade");
    let blob = if env::var_os("CARGO_FEATURE_COMPILER").is_some() {
        lumen::precompiled::precompile_glue(&glue, "performance-timeline")
    } else {
        lumen_aot::native::precompile_glue_for_build(&glue, "performance-timeline")
    }.unwrap_or_else(|error| panic!("Performance facade failed to precompile: {error}"));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("performance_timeline.aot"), blob).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
