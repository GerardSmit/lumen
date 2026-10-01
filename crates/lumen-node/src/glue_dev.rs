//! `LUMEN_NODE_GLUE_DIR=<dir>`: assemble the glue from `<dir>/*.js` at startup instead of loading
//! the precompiled blob, so a change to the JS builtins takes effect without a rebuild. Point it
//! at `crates/lumen-node/src/js`. Startup is slower (the glue is parsed every run); the file list
//! is the one this binary was built with, so adding or removing a glue file still needs a build.

#[path = "glue.rs"]
mod glue;

include!(concat!(env!("OUT_DIR"), "/glue_manifest.rs"));

pub fn source() -> Option<&'static str> {
    let dir = std::path::PathBuf::from(std::env::var_os("LUMEN_NODE_GLUE_DIR")?);
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    let feature_on = |f: &str| match f {
        "HTTP2" => cfg!(feature = "http2"),
        "CLUSTER" => cfg!(feature = "cluster"),
        "DGRAM" => cfg!(feature = "dgram"),
        "WASI" => cfg!(feature = "wasi"),
        "BUN" => cfg!(feature = "bun"),
        other => panic!("glue feature {other} has no cfg mapping in glue_dev.rs"),
    };
    let mem_marks = std::env::var_os("LUMEN_GLUE_MEM_MARKS").is_some();
    let glue = glue::assemble(MANIFEST, read, feature_on, mem_marks);
    Some(Box::leak(glue.into_boxed_str()))
}
