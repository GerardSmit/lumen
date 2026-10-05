use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("../lumen-node/src/js/webstreams_browser.js");
    println!("cargo:rerun-if-changed={}", source.display());
    let glue = fs::read_to_string(&source)
        .unwrap_or_else(|error| panic!("read {}: {error}", source.display()));
    let blob = if env::var_os("CARGO_FEATURE_COMPILER").is_some() {
        lumen::precompiled::precompile_glue(&glue, "web-streams-glue")
    } else {
        lumen_aot::native::precompile_glue_for_build(&glue, "web-streams-glue")
    }
    .unwrap_or_else(|error| panic!("web streams glue failed to precompile: {error}"));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("webstreams_browser.js"), glue).unwrap();
    fs::write(out.join("webstreams_browser.aot"), blob).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
