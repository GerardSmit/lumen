//! Precompile the web platform's JS glue to an AST snapshot at build time, so the runtime
//! *decodes* it on boot instead of re-lexing/parsing it — parsing the static glue is the
//! dominant cold-start cost. This assembles the one IIFE that used to live as a `concat!` in
//! `lib.rs` and writes both the assembled source (`web_glue.js`) and its snapshot blob
//! (`web_glue.snap`) to `OUT_DIR`; `lib.rs` `include_*!`s both. Assembling here makes this the
//! single source of truth — the source and its snapshot can't drift.
//!
//! `lumen` is a build-dependency purely for `compile_snapshot` (parse + encode). If the glue
//! ever fails to parse, the build fails here with a clear message.

use std::path::PathBuf;

/// Order matters: `preamble` captures and deletes the raw `__*` namespaces before the standard
/// classes are defined over them. Keep in sync with what each file expects.
///
/// A group of files that share top-level bindings (or patch each other's classes) is one unit;
/// a `lazy` unit's code runs the first time one of the globals it publishes is touched (see
/// `__lazyWeb` in preamble.js), so a program pays only for the web APIs it uses. Eager units
/// are the tiny ones other glue reads at startup.
struct Unit {
    files: &'static [&'static str],
    lazy: bool,
    /// Globals published by `Object.defineProperty` rather than `globalThis.X = …`: name and
    /// whether it is enumerable.
    defined: &'static [(&'static str, bool)],
}

const UNITS: &[Unit] = &[
    Unit {
        files: &["preamble.js"],
        lazy: false,
        defined: &[],
    },
    Unit {
        files: &["encoding.js", "serialize.js"],
        lazy: true,
        defined: &[],
    },
    Unit {
        files: &["urlpattern.js"],
        lazy: true,
        defined: &[],
    },
    Unit {
        files: &["fetch.js"],
        lazy: true,
        defined: &[],
    },
    Unit {
        files: &["server.js"],
        lazy: false,
        defined: &[],
    },
    Unit {
        files: &["navigator.js"],
        lazy: false,
        defined: &[],
    },
    Unit {
        files: &["wasm.js"],
        lazy: true,
        defined: &[],
    },
];

/// The globals a lazy unit publishes: its column-0 `globalThis.X = …` statements, then `defined`.
fn assigned_export(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("globalThis.")?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    let assigned = rest[end..].trim_start().strip_prefix('=')
        .is_some_and(|tail| !tail.starts_with('='));
    (end > 0 && assigned).then_some(&rest[..end])
}

fn published(body: &str, defined: &[(&str, bool)]) -> Vec<(String, bool)> {
    let mut names: Vec<(String, bool)> = Vec::new();
    for line in body.lines() {
        let Some(name) = assigned_export(line) else {
            continue;
        };
        if !names.iter().any(|(n, _)| n == name) {
            names.push((name.to_string(), true));
        }
    }
    for (name, enumerable) in defined {
        if !names.iter().any(|(n, _)| n == name) {
            names.push((name.to_string(), *enumerable));
        }
    }
    assert!(
        !names.is_empty(),
        "a lazy web glue unit must publish a global"
    );
    names
}

/// Route publications through the unit's private receiver. Reads and closures
/// still use the real global. The same scanner discovers and redirects exports,
/// so a unit cannot overwrite a later native provider or author replacement.
fn guarded_publications(body: &str) -> String {
    let mut guarded = String::with_capacity(body.len());
    for line in body.lines() {
        if assigned_export(line).is_some() {
            guarded.push_str("__webExports.");
            guarded.push_str(line.strip_prefix("globalThis.").unwrap());
        } else {
            guarded.push_str(line);
        }
        guarded.push('\n');
    }
    // This also covers the URL unit's dynamic names and internal helper
    // descriptors. The publisher preserves non-export helper semantics.
    guarded.replace("Object.defineProperty(globalThis,", "__webDefine(")
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest.join("src/js");

    let mut glue = String::from("(() => {\n");
    for unit in UNITS {
        let mut body = String::new();
        for file in unit.files {
            let path = src_dir.join(file);
            println!("cargo:rerun-if-changed={}", path.display());
            body.push_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
            );
            body.push('\n');
        }
        if unit.lazy {
            let names = published(&body, unit.defined);
            let body = guarded_publications(&body);
            let list: Vec<String> = names
                .iter()
                .map(|(n, e)| format!("{n}{}", if *e { "" } else { "!" }))
                .collect();
            glue.push_str(&format!(
                "__lazyWeb({:?}, (__webExports, __webDefine) => {{\n\"lumen:run-once\";\n{body}}});\n",
                list.join(" ")
            ));
        } else {
            glue.push_str(&body);
            if unit.files == ["preamble.js"] && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
                glue.push_str("__http.policyHandledByHost = true;\n");
            }
        }
    }
    glue.push_str("\n})();");

    // An ahead-of-time blob: AST, bytecode and the compressed function text (for `toString`).
    let blob = if std::env::var_os("CARGO_FEATURE_COMPILER").is_some() {
        lumen::precompiled::precompile_glue(&glue, "web-glue")
    } else {
        lumen_aot::native::precompile_glue_for_build(&glue, "web-glue")
    }
    .unwrap_or_else(|e| panic!("web glue failed to precompile: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // web_glue.js is for reference only (not linked).
    std::fs::write(out.join("web_glue.js"), &glue).unwrap();
    std::fs::write(out.join("web_glue.aot"), &blob).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
