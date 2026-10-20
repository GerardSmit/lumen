use std::{env, fs, path::PathBuf};

/// The shared Performance facade lives in `lumen-host`, so the glue is read from the workspace.
const GLUE_PATH: &str = "../lumen-host/src/performance_timeline.js";

/// Bytecode when the `compiler` feature is on, otherwise a native AOT blob with `aot-native`.
/// Without either (or when the target has no native AOT) the blob is empty and the extension
/// evaluates the glue source instead.
fn precompile(glue: &str) -> Vec<u8> {
    #[cfg(feature = "compiler")]
    {
        return lumen::precompiled::precompile_glue(glue, "html-performance-timeline")
            .unwrap_or_else(|error| panic!("Performance facade failed to precompile: {error}"));
    }
    #[cfg(all(feature = "aot-native", not(feature = "compiler")))]
    {
        return match lumen_aot::native::precompile_glue_for_build(glue, "html-performance-timeline") {
            Ok(blob) => blob,
            Err(error) => {
                println!("cargo:warning=Performance facade has no native AOT blob ({error}); it is evaluated from source");
                Vec::new()
            }
        };
    }
    #[cfg(not(any(feature = "compiler", feature = "aot-native")))]
    {
        let _ = glue;
        Vec::new()
    }
}

fn main() {
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join(GLUE_PATH);
    println!("cargo:rerun-if-changed={}", source.display());
    let glue = fs::read_to_string(&source).unwrap_or_else(|error| {
        panic!("lumen-html-js reads the shared Performance facade from {} (the lumen workspace layout is required): {error}", source.display())
    });
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("html_performance_timeline.aot"), precompile(&glue)).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
